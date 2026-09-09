use std::fs::read_dir;
use std::path::Path;
use std::path::PathBuf;

use std::collections::HashMap;

use axum::Form;
use axum::extract::Path as AxumPath;
use axum::extract::Query;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Html;
use axum::response::Redirect;
use serde::Deserialize;

use crate::cmd::run_blocking;
use crate::cmd::serve::auth::CurrentUser;
use crate::cmd::serve::cards::COLLECTION_META_FILE;
use crate::cmd::serve::cards::CardRoot;
use crate::cmd::serve::cards::IdPolicy;
use crate::cmd::serve::cards::collection_id;
use crate::cmd::serve::cards::discover_local_collections;
use crate::cmd::serve::cards::existing_collection_id;
use crate::cmd::serve::cards::user_db_path;
use crate::cmd::serve::config::ResolvedCollection;
use crate::cmd::serve::config::slugify;
use crate::cmd::serve::edit::build_card_migration;
use crate::cmd::serve::edit::file_mtime_ms;
use crate::cmd::serve::edit::plan_hash_migration;
use crate::cmd::serve::edit::revert_file;
use crate::cmd::serve::edit::write_atomic;
use crate::cmd::serve::files_ui::render_editor_page;
use crate::cmd::serve::files_ui::render_preview;
use crate::cmd::serve::files_ui::render_tree_page;
use crate::cmd::serve::href::encoded_path;
use crate::cmd::serve::state::AppState;
use crate::cmd::serve::state::migrate_sessions;
use crate::cmd::serve::state::sessions_touching;
use crate::cmd::serve::upload::MEDIA_DIR;
use crate::db::Database;
use crate::error::Fallible;
use crate::error::fail;
use crate::flash::Flash;
use crate::media::validate::validate_media_files;
use crate::parser::ParsedFile;
use crate::parser::Parser;
use crate::parser::strip_frontmatter_with_offset;
use crate::types::card::Card;
use crate::types::collection_id::CollectionId;
use crate::types::performance::Performance;
use crate::types::timestamp::Timestamp;
use crate::user_db::UserDatabase;
use crate::utils::ensure_dir;

/// Seed content for a new card file, also offered for copying on the
/// Sources page. Frontmatter is TOML, matching `parse_deck`.
pub const CARD_TEMPLATE: &str = r#"---
name = "My deck"
---

Q: What is the capital of France?
A: Paris.

C: The mitochondria is the [powerhouse] of the cell.
"#;

/// One row of the file tree, flattened depth-first for rendering.
pub struct TreeEntry {
    pub rel_path: String,
    pub name: String,
    pub is_dir: bool,
    pub depth: usize,
}

/// The whole tree under `root`, folders before files at each level, each
/// level sorted by name. Dotfiles, the per-collection metadata file and a
/// collection's `media` folder are hidden: they are hashcards' bookkeeping,
/// not the user's content.
pub fn read_tree(root: &CardRoot) -> Fallible<Vec<TreeEntry>> {
    let mut out = Vec::new();
    walk(root.path(), "", 0, &mut out)?;
    Ok(out)
}

fn walk(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<TreeEntry>) -> Fallible<()> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in read_dir(dir)? {
        let entry = entry?;
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if name.starts_with('.') || name == COLLECTION_META_FILE {
            continue;
        }
        // `media` directly inside a collection is where pasted images are
        // stored, addressed only through the cards that reference them. A
        // top-level folder of that name is the user's own collection, so
        // only the depth makes it bookkeeping.
        if depth == 1 && name == MEDIA_DIR {
            continue;
        }
        let path = entry.path();
        if path.is_symlink() {
            continue;
        }
        if path.is_dir() {
            dirs.push(name);
        } else if name.ends_with(".md") {
            files.push(name);
        }
    }
    dirs.sort();
    files.sort();

    for name in dirs {
        let rel_path = join_rel(prefix, &name);
        out.push(TreeEntry {
            rel_path: rel_path.clone(),
            name: name.clone(),
            is_dir: true,
            depth,
        });
        walk(&dir.join(&name), &rel_path, depth + 1, out)?;
    }
    for name in files {
        out.push(TreeEntry {
            rel_path: join_rel(prefix, &name),
            name,
            is_dir: false,
            depth,
        });
    }
    Ok(())
}

fn join_rel(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

/// A new file or folder name: exactly one path component, and not one of
/// hashcards' own bookkeeping names.
fn validate_name(name: &str) -> Fallible<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return fail("Enter a name.");
    }
    if trimmed == COLLECTION_META_FILE {
        return fail(format!(
            "`{COLLECTION_META_FILE}` is reserved by hashcards."
        ));
    }
    if trimmed.starts_with('.') {
        return fail("Names cannot start with a dot.");
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return fail("Names cannot contain `/` or `\\` — create a folder instead.");
    }
    if PathBuf::from(trimmed).components().count() != 1 {
        return fail(format!("`{trimmed}` is not a valid name."));
    }
    Ok(trimmed.to_string())
}

/// Refuse a folder that would take the name hashcards keeps a collection's
/// pasted images under.
///
/// `read_tree` hides such a folder and `non_empty_children` does not count
/// it, both on the understanding that it is ours. A folder of the user's own
/// under that name would therefore be invisible in the tree *and* invisible
/// to the "refuse a non-empty folder" guard, so deleting the collection
/// would take the decks inside it — and their review database — with it.
///
/// Only folders, and only directly inside a collection: a deck called
/// `media.md` is fine, and a collection of the user's own called `media` is
/// their folder rather than ours.
/// Whether `dir` holds a card file anywhere below it.
///
/// Asked of a collection's `media` folder before the collection is deleted:
/// that folder is ours, so nothing lists it and nothing counts it, and a
/// folder made by hand under that name before the name was reserved would
/// otherwise be destroyed without ever having been shown.
fn holds_decks(dir: &Path) -> Fallible<bool> {
    if !dir.is_dir() {
        return Ok(false);
    }
    for entry in read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_symlink() {
            continue;
        }
        if path.is_dir() {
            if holds_decks(&path)? {
                return Ok(true);
            }
        } else if entry.file_name().to_string_lossy().ends_with(".md") {
            return Ok(true);
        }
    }
    Ok(false)
}

fn check_media_name(name: &str, is_dir: bool, parent_is_collection_root: bool) -> Fallible<()> {
    if is_dir && parent_is_collection_root && name == MEDIA_DIR {
        return fail(format!(
            "`{MEDIA_DIR}` is where hashcards keeps a collection's pasted images. Pick a different folder name."
        ));
    }
    Ok(())
}

/// Names in `dir` that belong to the user, ignoring hashcards' own
/// bookkeeping. A folder holding only bookkeeping counts as empty.
///
/// `is_collection_root` says whether a `media` folder here is ours: the
/// images belong to the cards, so a collection whose decks are all gone is
/// empty even while their pictures are still on disk.
fn non_empty_children(dir: &Path, is_collection_root: bool) -> Fallible<Vec<String>> {
    let mut kept = Vec::new();
    for entry in read_dir(dir)? {
        let name = entry?.file_name().into_string().unwrap_or_default();
        if name.starts_with('.') || name == COLLECTION_META_FILE {
            continue;
        }
        if is_collection_root && name == MEDIA_DIR {
            continue;
        }
        kept.push(name);
    }
    kept.sort();
    Ok(kept)
}

/// The local tree belonging to the caller, created if it is not there yet.
pub fn user_root(state: &AppState, user: Option<&CurrentUser>) -> Fallible<CardRoot> {
    let data_dir = data_dir(state)?;
    CardRoot::for_user(&data_dir, user.map(|u| u.email.as_str()))
}

/// The same tree, without creating anything. Read paths use this: serving a
/// page must not write into the user's card folder, and must not fail on a
/// read-only data directory.
pub fn user_root_readonly(state: &AppState, user: Option<&CurrentUser>) -> Fallible<CardRoot> {
    let data_dir = data_dir(state)?;
    CardRoot::open(&data_dir, user.map(|u| u.email.as_str()))
}

fn data_dir(state: &AppState) -> Fallible<PathBuf> {
    match &state.config.data_dir {
        Some(d) => Ok(d.clone()),
        None => fail(
            "Local card folders need a data directory. Start hashcards-web with a config file.",
        ),
    }
}

#[derive(Deserialize)]
pub struct NewEntryForm {
    /// Parent folder, relative to the user's root. Empty means the root.
    pub parent: String,
    pub name: String,
}

#[derive(Deserialize)]
pub struct RenameForm {
    pub path: String,
    pub name: String,
}

#[derive(Deserialize)]
pub struct DeleteForm {
    pub path: String,
}

/// Every file-manager handler walks the tree, and most of them also open
/// SQLite. That is blocking work (BUG-44), so none of it runs on the async
/// executor.
pub async fn files_get_handler(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    current_user: Option<CurrentUser>,
) -> (StatusCode, Html<String>) {
    let flash = Flash::from_query(&query);
    let tree = run_blocking(move || {
        let root = user_root(&state, current_user.as_ref())?;
        read_tree(&root)
    })
    .await;
    let markup = match tree {
        Ok(tree) => render_tree_page(&tree, flash),
        Err(e) => render_tree_page(&[], Some(Flash::error(e.to_string()))),
    };
    (StatusCode::OK, Html(markup.into_string()))
}

pub async fn files_folder_handler(
    State(state): State<AppState>,
    current_user: Option<CurrentUser>,
    Form(form): Form<NewEntryForm>,
) -> Redirect {
    flash_for(run_blocking(move || create_entry(&state, current_user.as_ref(), &form, true)).await)
}

pub async fn files_file_handler(
    State(state): State<AppState>,
    current_user: Option<CurrentUser>,
    Form(form): Form<NewEntryForm>,
) -> Redirect {
    flash_for(run_blocking(move || create_entry(&state, current_user.as_ref(), &form, false)).await)
}

pub async fn files_rename_handler(
    State(state): State<AppState>,
    current_user: Option<CurrentUser>,
    Form(form): Form<RenameForm>,
) -> Redirect {
    flash_for(run_blocking(move || rename_entry(&state, current_user.as_ref(), &form)).await)
}

pub async fn files_delete_handler(
    State(state): State<AppState>,
    current_user: Option<CurrentUser>,
    Form(form): Form<DeleteForm>,
) -> Redirect {
    flash_for(run_blocking(move || delete_entry(&state, current_user.as_ref(), &form)).await)
}

/// Every file-manager mutation reports back on `/files` the same way.
fn flash_for(outcome: Fallible<String>) -> Redirect {
    match outcome {
        Ok(msg) => Flash::success(msg).redirect("/files"),
        Err(e) => Flash::error(e.to_string()).redirect("/files"),
    }
}

/// Slugs a card folder must not take: the caller's own saved decks. Both a
/// deck and a collection are addressed through `/collection/{slug}`, and
/// routing prefers the collection, so a folder that matches a deck slug
/// makes the deck unreachable.
fn reserved_slugs(state: &AppState, owner: Option<&str>) -> Vec<(String, String)> {
    state
        .custom_decks
        .lock()
        .iter()
        .filter(|d| d.owner.as_deref() == owner)
        .map(|d| (d.slug.clone(), d.name.clone()))
        .collect()
}

/// The caller's owner key, as stored on a `ResolvedCollection`.
fn owner_key(user: Option<&CurrentUser>) -> Option<String> {
    user.map(|u| u.email.to_lowercase())
}

/// Refuse a top-level folder name that would collide with an existing
/// collection's URL slug.
///
/// Only top-level folders are collections, so only they share the slug
/// namespace. `except` is the folder being renamed, which does not collide
/// with itself.
fn check_collection_slug(
    state: &AppState,
    user: Option<&CurrentUser>,
    root: &CardRoot,
    name: &str,
    except: Option<&Path>,
) -> Fallible<()> {
    let slug = slugify(name);
    let owner = owner_key(user);
    let mut taken = reserved_slugs(state, owner.as_deref());
    let db_dir = match &state.config.data_dir {
        Some(d) => d.join("db"),
        None => return fail("No data directory is configured."),
    };
    // `CreateMissing`, not `ExistingOnly`: a folder dropped in by hand has
    // no id yet, and it still owns its slug.
    let siblings =
        discover_local_collections(root, &db_dir, owner.as_deref(), IdPolicy::CreateMissing)?;
    taken.extend(
        siblings
            .into_iter()
            .filter(|c| Some(c.coll_dir.as_path()) != except)
            .map(|c| (c.slug.clone(), c.name)),
    );
    if let Some((_, other)) = taken.iter().find(|(s, _)| *s == slug) {
        return fail(format!(
            "`{name}` maps to the URL slug `{slug}`, which `{other}` already uses. Pick a different name."
        ));
    }
    Ok(())
}

fn create_entry(
    state: &AppState,
    user: Option<&CurrentUser>,
    form: &NewEntryForm,
    is_dir: bool,
) -> Fallible<String> {
    let root = user_root(state, user)?;
    let mut name = validate_name(&form.name)?;
    if !is_dir && !name.ends_with(".md") {
        name.push_str(".md");
    }
    let parent = form.parent.trim().trim_matches('/');
    // Normalized here, so `parent` and the path that is actually created
    // cannot disagree about which collection the new entry lands in.
    let entry = root.resolve_entry(&join_rel(parent, &name))?;
    let rel = entry.rel;
    let target = entry.path;
    if target.exists() {
        return fail(format!("`{rel}` already exists."));
    }
    let parent_rel = match rel.rsplit_once('/') {
        Some((parent, _)) => parent,
        None => "",
    };
    check_media_name(
        &name,
        is_dir,
        !parent_rel.is_empty() && !parent_rel.contains('/'),
    )?;
    // A collection is a top-level folder, so a file directly in the root
    // would have no review database and no way to serve its images —
    // exactly what `LOOSE_FILE` says. Refused here rather than created and
    // then tripped over: `collection_id` on a regular file fails with
    // `ENOTDIR`, reporting an OS error for something already on disk.
    if !is_dir && parent_rel.is_empty() {
        return fail(LOOSE_FILE_NEW);
    }

    // Both branches create every missing ancestor, so either can bring a
    // top-level folder into being — a file under `parent=New/Deep` as much
    // as a folder created at the root. That folder is a collection, and one
    // whose slug is already taken is dropped from the listing with nothing
    // but a log line to say why, so the name has to be checked wherever it
    // comes from rather than only on the root form.
    let top = match rel.split('/').next() {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => return fail(format!("`{rel}` is not a valid path.")),
    };
    let top_path = root.resolve(&top)?;
    let new_collection = !top_path.exists();
    if new_collection {
        check_collection_slug(state, user, &root, &top, None)?;
    }

    if is_dir {
        std::fs::create_dir_all(&target)?;
    } else {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, CARD_TEMPLATE)?;
    }
    // Give a new top-level folder its id immediately, so the collection it
    // becomes keeps its database across a later rename.
    if new_collection && top_path.is_dir() {
        collection_id(&top_path)?;
    }
    if is_dir {
        Ok(format!("Created folder `{rel}`."))
    } else {
        Ok(format!("Created `{rel}`."))
    }
}

fn rename_entry(
    state: &AppState,
    user: Option<&CurrentUser>,
    form: &RenameForm,
) -> Fallible<String> {
    let root = user_root(state, user)?;
    let entry = root.resolve_entry(&form.path)?;
    let from_rel = entry.rel;
    let from = entry.path;
    if !from.exists() {
        return fail(format!("`{from_rel}` does not exist."));
    }
    // Renaming a deck rewrites nothing, but renaming its *collection* moves
    // the folder a live session is reading its cards and its database from.
    refuse_if_drilling(state, &root, &from_rel)?;
    let mut name = validate_name(&form.name)?;
    if from.is_file() && !name.ends_with(".md") {
        name.push_str(".md");
    }
    let parent_rel = match from_rel.rsplit_once('/') {
        Some((parent, _)) => parent.to_string(),
        None => String::new(),
    };
    check_media_name(
        &name,
        from.is_dir(),
        !parent_rel.is_empty() && !parent_rel.contains('/'),
    )?;
    let to_rel = join_rel(&parent_rel, &name);
    let to = root.resolve(&to_rel)?;
    if to.exists() {
        return fail(format!("`{to_rel}` already exists."));
    }
    if from.is_dir() && parent_rel.is_empty() {
        check_collection_slug(state, user, &root, &name, Some(from.as_path()))?;
    }
    std::fs::rename(&from, &to)?;
    Ok(format!("Renamed to `{to_rel}`."))
}

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
    // its grades to the collection's database. Deleting either underneath it
    // strands those grades — the database file is unlinked while the session
    // still holds it open — so the same guard `save_file` uses applies here.
    refuse_if_drilling(state, &root, &rel)?;
    if target.is_dir() {
        let is_collection_root = !rel.contains('/');
        // `media` does not make a collection count as non-empty, and it is
        // hidden from the tree — both because hashcards put it there. One
        // made by hand before that name was reserved can hold decks, and
        // those would go with the collection having never been listed at
        // all. Say where they are instead of deleting them.
        if is_collection_root && holds_decks(&target.join(MEDIA_DIR))? {
            return fail(format!(
                "`{rel}/{MEDIA_DIR}` holds card files. That folder is where hashcards keeps a \
                 collection's pasted images, so it is not shown here — move the files out of it \
                 from outside hashcards before deleting this collection."
            ));
        }
        // Refuse a non-empty folder: deleting a whole collection on a
        // misclick would take its review history with it.
        let kept = non_empty_children(&target, is_collection_root)?;
        if !kept.is_empty() {
            return fail(format!(
                "`{rel}` is not empty — it still holds {}. Delete those first.",
                kept.join(", ")
            ));
        }
        // A top-level folder is a collection: its `.hashcards.toml` goes
        // with it, so leaving `{id}.db` behind would orphan a database that
        // nothing can ever address again — and a folder recreated under the
        // same name would silently start its history over.
        //
        // The id is read first because it lives in the folder, but the
        // database is removed *after* it: if `remove_dir_all` fails, the
        // collection is still there and must still have its history.
        let id = if is_collection_root {
            existing_collection_id(&target)?
        } else {
            None
        };
        std::fs::remove_dir_all(&target)?;
        if let Some(id) = id {
            remove_collection_database(state, &id)?;
        }
    } else {
        std::fs::remove_file(&target)?;
    }
    Ok(format!("Deleted `{rel}`."))
}

/// Delete the review database of the collection whose id is `id`.
///
/// Called after the folder itself is gone: a folder with no id never had a
/// database to begin with, and one whose removal failed still needs its
/// history.
fn remove_collection_database(state: &AppState, id: &CollectionId) -> Fallible<()> {
    let db_dir = match &state.config.data_dir {
        Some(d) => d.join("db"),
        None => return Ok(()),
    };
    // SQLite leaves the write-ahead log and shared-memory files beside the
    // database; removing only the database would strand them.
    for suffix in ["", "-wal", "-shm"] {
        let path = db_dir.join(format!("{id}.db{suffix}"));
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Why a file directly in the user's root can be neither drilled nor
/// edited: only a top-level folder is a collection, so such a file has no
/// review database to save into and no slug to serve its images from.
pub const LOOSE_FILE: &str = "This file sits directly in your card folder, so it belongs to no \
     collection — it has nowhere to keep its review history and no way to serve its images. \
     Move it into a collection folder first.";

/// Why a card file cannot be created directly in the user's root: it would
/// be that same loose file, before anyone has typed a card into it.
pub const LOOSE_FILE_NEW: &str = "A card file has to live inside a collection folder, so that it \
     has somewhere to keep its review history and a way to serve its images. Create a collection \
     first, then add the file to it.";

/// The collection folder a local file belongs to.
///
/// A collection is a top-level folder, so a file directly under the root
/// belongs to none: it can be neither reviewed nor given a place to keep
/// its images, and every caller refuses it rather than inventing one.
pub fn collection_folder(root: &CardRoot, rel: &str) -> Fallible<PathBuf> {
    let trimmed = rel.trim_matches('/');
    let top = match trimmed.split('/').next() {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => return fail(format!("`{rel}` is not inside a collection folder.")),
    };
    if !trimmed.contains('/') {
        return fail(LOOSE_FILE);
    }
    let folder = root.resolve(&top)?;
    if !folder.is_dir() {
        return fail(format!("`{top}` is not a collection folder."));
    }
    Ok(folder)
}

/// Where a save to the local collection at `coll_dir` writes: the owning
/// user's review database, and the id that scopes this collection's rows
/// inside it.
///
/// The id comes from the folder's `.hashcards.toml` rather than its slug, so
/// renaming a collection keeps its history.
pub fn db_target_for(
    root: &CardRoot,
    coll_dir: &Path,
    db_dir: &Path,
) -> Fallible<(PathBuf, CollectionId)> {
    Ok((user_db_path(root, db_dir)?, collection_id(coll_dir)?))
}

/// Parse an unsaved buffer as the file at `rel` would be parsed on disk.
///
/// The deck name comes from the filename, the line offset from the TOML
/// frontmatter, so reported error lines match what is in the textarea.
pub fn parse_buffer(rel: &str, content: &str) -> Fallible<ParsedFile> {
    parse_buffer_at(rel, &PathBuf::from(rel), content)
}

/// The same, but naming the file by where it actually is on disk.
///
/// `validate_media_files` canonicalizes each card's path to place it inside
/// its collection, which only the absolute path can answer — the relative
/// one would be resolved against the process's working directory.
fn parse_buffer_at(rel: &str, file_path: &Path, content: &str) -> Fallible<ParsedFile> {
    let (body, offset) = strip_frontmatter_with_offset(content)?;
    let deck_name = rel
        .rsplit('/')
        .next()
        .unwrap_or(rel)
        .trim_end_matches(".md")
        .to_string();
    let parser = Parser::new(deck_name, file_path.to_path_buf(), offset);
    Ok(parser.parse_with_duplicates(body)?)
}

/// The folder of the collection a normalized path belongs to, if it belongs
/// to one.
///
/// Unlike `collection_folder`, the path may *be* the collection folder:
/// deleting or renaming a collection touches its cards just as much as
/// editing one of them does. A loose file in the root belongs to no
/// collection, so nothing can be drilling it.
fn owning_collection_folder(root: &CardRoot, rel: &str) -> Option<PathBuf> {
    let top = rel.split('/').next().filter(|t| !t.is_empty())?;
    let folder = root.resolve(top).ok()?;
    folder.is_dir().then_some(folder)
}

/// Refuse a change to a collection somebody is drilling right now.
///
/// Migration handles *edits*; it cannot handle a collection or file that
/// stopped existing. A session cannot be re-keyed onto cards that are gone,
/// and deleting a collection unlinks the database the session still holds
/// open, so every grade it writes after that goes to a file nothing can
/// read back.
///
/// `rel` must already be normalized, so that where the file is and which
/// collection owns it are answered from the same string.
fn refuse_if_drilling(state: &AppState, root: &CardRoot, rel: &str) -> Fallible<()> {
    let Some(folder) = owning_collection_folder(root, rel) else {
        return Ok(());
    };
    if sessions_touching(state, &folder).is_empty() {
        return Ok(());
    }
    fail("A drill session is using this collection's cards. End it before changing its files.")
}

#[derive(Deserialize)]
pub struct SaveForm {
    pub content: String,
    /// The mtime the browser loaded, so a save cannot silently overwrite a
    /// change made elsewhere. Same guard the card editor uses.
    pub mtime: u64,
}

pub async fn editor_get_handler(
    State(state): State<AppState>,
    AxumPath(rel): AxumPath<String>,
    Query(query): Query<HashMap<String, String>>,
    current_user: Option<CurrentUser>,
) -> (StatusCode, Html<String>) {
    let flash = Flash::from_query(&query);
    let rel2 = rel.clone();
    let loaded = run_blocking(move || load_for_edit(&state, current_user.as_ref(), &rel2)).await;
    let markup = match loaded {
        Ok((content, mtime)) => render_editor_page(&rel, &content, mtime, flash),
        Err(e) => render_editor_page(&rel, "", 0, Some(Flash::error(e.to_string()))),
    };
    (StatusCode::OK, Html(markup.into_string()))
}

fn load_for_edit(
    state: &AppState,
    user: Option<&CurrentUser>,
    rel: &str,
) -> Fallible<(String, u64)> {
    let root = user_root(state, user)?;
    let entry = root.resolve_entry(rel)?;
    let rel = &entry.rel;
    let path = entry.path;
    if !path.is_file() {
        return fail(format!("`{rel}` is not a file."));
    }
    // Refused here rather than on save: a file outside a collection cannot
    // be saved and cannot have its images previewed, so opening an editor
    // over it would be a page that does not work.
    collection_folder(&root, rel)?;
    let content = std::fs::read_to_string(&path)?;
    let mtime = file_mtime_ms(&path)?;
    Ok((content, mtime))
}

pub async fn editor_post_handler(
    State(state): State<AppState>,
    AxumPath(rel): AxumPath<String>,
    current_user: Option<CurrentUser>,
    Form(form): Form<SaveForm>,
) -> Result<Redirect, (StatusCode, Html<String>)> {
    let rel2 = rel.clone();
    let content = form.content.clone();
    let submitted_mtime = form.mtime;
    // Parsing the buffer, validating its media and applying the hash
    // migration are all blocking work (BUG-44). So is re-reading the mtime a
    // refusal needs, so that happens on the same trip rather than putting
    // the blocking work straight back on the executor.
    //
    // The mtime is re-read rather than echoed back: every refusal leaves the
    // original content on disk, but `revert_file` rewriting it moves the
    // mtime on, and a stale one would make the next save fail as a phantom
    // conflict. On a real conflict this is the mtime of the change that
    // landed, so saving again deliberately overwrites it — which is what the
    // message says.
    let outcome = run_blocking(move || {
        Ok(
            match save_file(&state, current_user.as_ref(), &rel2, &form) {
                Ok(msg) => Ok(msg),
                Err(e) => {
                    let mtime = user_root(&state, current_user.as_ref())
                        .and_then(|root| root.resolve(&rel2))
                        .and_then(|path| file_mtime_ms(&path))
                        .unwrap_or(submitted_mtime);
                    Err((e.to_string(), mtime))
                }
            },
        )
    })
    .await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(e) => Err((e.to_string(), submitted_mtime)),
    };

    match outcome {
        Ok(msg) => {
            let target = format!("/files/edit/{}", encoded_path(&rel));
            Ok(Flash::success(msg).redirect(&target))
        }
        // Redirecting here would send the browser to a GET that re-reads
        // the file from disk — which a refused save has already put back —
        // and the whole buffer would be gone. Answer with the editor over
        // the text that was submitted instead, so the fix is one edit away.
        Err((error, mtime)) => {
            let markup = render_editor_page(&rel, &content, mtime, Some(Flash::error(error)));
            Err((StatusCode::UNPROCESSABLE_ENTITY, Html(markup.into_string())))
        }
    }
}

/// Write the buffer, reparse it, and migrate card hashes so a reworded card
/// keeps its schedule. A buffer that does not parse never stays on disk.
fn save_file(
    state: &AppState,
    user: Option<&CurrentUser>,
    rel: &str,
    form: &SaveForm,
) -> Fallible<String> {
    let root = user_root(state, user)?;
    // Normalized before anything is asked of it: the slug below decides
    // whether a session is drilling these cards, and it has to be derived
    // from the same string the file itself is read from.
    let entry = root.resolve_entry(rel)?;
    let rel = &entry.rel;
    let path = entry.path;
    if !path.is_file() {
        return fail(format!("`{rel}` is not a file."));
    }

    let coll_dir = collection_folder(&root, rel)?;

    if file_mtime_ms(&path)? != form.mtime {
        return fail(
            "This file changed since you opened it, so it was not saved. Your edit is still \
             here — check it against the other change, then save again to overwrite that one.",
        );
    }

    let original = std::fs::read_to_string(&path)?;
    // A file that does not parse any more — edited outside the UI, or
    // restored from elsewhere — has no cards to carry history forward from,
    // but it must still be repairable here rather than rejecting every save
    // with the old content's error.
    let old_cards = match parse_buffer_at(rel, &path, &original) {
        Ok(parsed) => parsed.cards,
        Err(e) => {
            log::warn!(
                "`{rel}` did not parse before this edit, so no review history is carried over: {e}"
            );
            Vec::new()
        }
    };

    // Everything that can fail without looking at the new content happens
    // before the file is touched: a save that reports an error must never
    // leave the rewritten file behind.
    let db_dir = match &state.config.data_dir {
        Some(d) => d.join("db"),
        None => return fail("No data directory is configured."),
    };
    ensure_dir(&db_dir, "review database directory")?;
    let (db_path, id) = db_target_for(&root, &coll_dir, &db_dir)?;
    let db = UserDatabase::open(&db_path)?.collection(id);

    write_atomic(&path, &form.content)?;
    let new_cards = match parse_buffer_at(rel, &path, &form.content) {
        Ok(parsed) => parsed.cards,
        Err(e) => {
            revert_file(&path, &original)?;
            return fail(format!("Not saved — {}", e.message()));
        }
    };
    // `Collection::open` validates media when it loads, so a
    // reference to a file that is not there does not merely render as a
    // broken image: it fails the whole collection, taking its page, its
    // deck tree, its stats and the landing count down with it. Refused
    // here, exactly like a parse error, so the editor is never the way that
    // happens.
    if let Err(e) = validate_media_files(&new_cards, &coll_dir) {
        revert_file(&path, &original)?;
        return fail(format!("Not saved — {}", e.message()));
    }
    let old_refs: Vec<&Card> = old_cards.iter().collect();
    let new_refs: Vec<&Card> = new_cards.iter().collect();
    let plan = plan_hash_migration(&old_refs, &new_refs);
    // One transaction. If it fails, put the file back rather than leaving a
    // rewritten file behind a half-migrated database.
    let counts = match db.apply_edit_migration(&plan.renames, &plan.fresh, Timestamp::now()) {
        Ok(counts) => counts,
        Err(e) => {
            revert_file(&path, &original)?;
            return fail(format!(
                "Not saved — the review history could not be updated: {e}"
            ));
        }
    };

    // Only mention unmatched cards when there was history to lose: on a file
    // nobody has drilled, every card is "unmatched" and saying so is noise.
    let skipped = plan.skipped + counts.collided;
    let worth_reporting = skipped > 0 && any_card_has_history(&db, &old_cards)?;
    let mut message = if worth_reporting {
        format!(
            "Saved {} cards. {skipped} could not be matched to their old review history and start fresh.",
            new_cards.len()
        )
    } else {
        format!("Saved {} cards.", new_cards.len())
    };

    // Written and committed: from here the session is re-keyed onto the
    // cards that now exist, and nothing left can fail.
    let old_refs: Vec<&Card> = old_cards.iter().collect();
    let migration = build_card_migration(&plan, &old_refs, &new_cards);
    let session = migrate_sessions(state, &coll_dir, &migration);
    if session.renamed > 0 || session.dropped > 0 {
        message.push_str(" Your running session was updated.");
    }
    if session.session_finished {
        message.push_str(" It has no cards left, so it is finished.");
    }
    Ok(message)
}

#[derive(Deserialize)]
pub struct PreviewForm {
    pub path: String,
    pub content: String,
}

/// Parse and render an unsaved buffer for the editor's preview pane.
///
/// Runs the production parser deliberately: a JavaScript approximation
/// could render a card hashcards cannot actually read, which would be worse
/// than no preview at all. Never writes to disk.
pub async fn preview_handler(
    State(state): State<AppState>,
    current_user: Option<CurrentUser>,
    Form(form): Form<PreviewForm>,
) -> Html<String> {
    // The production parser reads the collection's media off disk, so this
    // is blocking work too (BUG-44).
    let rendered = run_blocking(move || {
        let root = user_root(&state, current_user.as_ref())?;
        Ok(render_preview(&root, &form.path, &form.content).into_string())
    })
    .await;
    match rendered {
        Ok(html) => Html(html),
        Err(e) => Html(maud::html! { div.preview-error { p { (e.to_string()) } } }.into_string()),
    }
}

/// Whether any of these cards has actually been reviewed.
///
/// Used to decide whether a save is worth warning about: replacing cards in
/// a file nobody has drilled loses nothing, and saying otherwise makes every
/// first edit of a freshly created file look alarming.
///
/// Note the predicate is `Reviewed`, not "a row exists": loading a
/// collection inserts a row per card, so `get_card_performance_opt` returns
/// `Some(Performance::New)` for cards nobody has ever seen.
fn any_card_has_history(db: &Database, cards: &[Card]) -> Fallible<bool> {
    for card in cards {
        if let Some(Performance::Reviewed(_)) = db.get_card_performance_opt(card.hash())? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The caller's local collections, discovered fresh.
///
/// Not cached: a folder created a moment ago must show up immediately, and
/// a directory listing is cheap next to the count refresh that follows it.
/// Discovery failures are logged and treated as "no local collections"
/// rather than taken down the whole page.
pub fn collections_for_user(
    state: &AppState,
    user: Option<&CurrentUser>,
) -> Vec<ResolvedCollection> {
    collections_for(state, user, IdPolicy::CreateMissing)
}

/// The caller's local collections as they already stand.
///
/// Read paths use this: a folder that has no id yet is skipped rather than
/// given one, so looking a collection up never writes into the user's tree
/// (and never blocks the async executor on creating it).
pub fn existing_collections_for_user(
    state: &AppState,
    user: Option<&CurrentUser>,
) -> Vec<ResolvedCollection> {
    collections_for(state, user, IdPolicy::ExistingOnly)
}

fn collections_for(
    state: &AppState,
    user: Option<&CurrentUser>,
    policy: IdPolicy,
) -> Vec<ResolvedCollection> {
    let data_dir = match &state.config.data_dir {
        Some(d) => d.clone(),
        None => return Vec::new(),
    };
    let root = match policy {
        IdPolicy::CreateMissing => user_root(state, user),
        IdPolicy::ExistingOnly => user_root_readonly(state, user),
    };
    let root = match root {
        Ok(r) => r,
        Err(e) => {
            log::error!("Cannot open the local card folder: {e}");
            return Vec::new();
        }
    };
    let owner = user.map(|u| u.email.as_str());
    match discover_local_collections(&root, &data_dir.join("db"), owner, policy) {
        Ok(found) => found,
        Err(e) => {
            log::error!("Cannot list local collections: {e}");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::serve::cards::CardRoot;
    use crate::helper::create_tmp_directory;
    use crate::types::collection_id::CollectionId;
    use crate::user_db::UserDatabase;

    #[test]
    fn tree_lists_folders_before_files_depth_first() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let root = CardRoot::for_user(&dir, None)?;
        std::fs::create_dir_all(root.path().join("Spanish"))?;
        std::fs::write(root.path().join("Spanish").join("verbs.md"), "Q: a\nA: b\n")?;
        std::fs::write(
            root.path().join("Spanish").join(COLLECTION_META_FILE),
            "id = \"x\"\n",
        )?;

        let tree = read_tree(&root)?;
        let paths: Vec<&str> = tree.iter().map(|e| e.rel_path.as_str()).collect();
        assert_eq!(paths, vec!["Spanish", "Spanish/verbs.md"]);
        assert!(tree[0].is_dir);
        assert_eq!(tree[1].depth, 1);
        Ok(())
    }

    #[test]
    fn tree_hides_the_metadata_file_and_dotfiles() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let root = CardRoot::for_user(&dir, None)?;
        std::fs::write(root.path().join(COLLECTION_META_FILE), "id = \"x\"\n")?;
        std::fs::write(root.path().join(".hidden"), "")?;

        assert!(read_tree(&root)?.is_empty());
        Ok(())
    }

    #[test]
    fn card_template_parses_into_two_cards() -> Fallible<()> {
        use crate::parser::Parser;
        use crate::parser::strip_frontmatter_with_offset;

        let (content, offset) = strip_frontmatter_with_offset(CARD_TEMPLATE)?;
        let parser = Parser::new("My deck".to_string(), PathBuf::from("t.md"), offset);
        let parsed = parser.parse_with_duplicates(content)?;
        assert_eq!(parsed.cards.len(), 2);
        Ok(())
    }

    #[test]
    fn names_must_be_single_components() {
        assert!(validate_name("verbs.md").is_ok());
        assert!(validate_name("Spanish").is_ok());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("..").is_err());
        assert!(validate_name("").is_err());
        assert!(validate_name(COLLECTION_META_FILE).is_err());
    }

    #[test]
    fn a_non_empty_folder_is_not_deleted() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let root = CardRoot::for_user(&dir, None)?;
        let folder = root.path().join("Spanish");
        std::fs::create_dir_all(&folder)?;
        std::fs::write(folder.join("verbs.md"), "Q: a\nA: b\n")?;
        // The metadata file alone must not count as "non-empty".
        std::fs::write(folder.join(COLLECTION_META_FILE), "id = \"x\"\n")?;

        assert_eq!(
            non_empty_children(&folder, true)?,
            vec!["verbs.md".to_string()]
        );

        std::fs::remove_file(folder.join("verbs.md"))?;
        assert!(non_empty_children(&folder, true)?.is_empty());
        Ok(())
    }

    #[test]
    fn sync_never_writes_into_the_local_root() -> Fallible<()> {
        // The local root must not sit under the directory that git and
        // source sync own, or a pull could overwrite user writing.
        let dir = create_tmp_directory()?;
        let root = CardRoot::for_user(&dir, None)?;
        assert!(!root.path().starts_with(dir.join("repo")));
        Ok(())
    }

    /// The database a save writes into is the *user's*, and which rows in it
    /// the save touches is the collection's id. Renaming the folder still
    /// changes neither.
    #[test]
    fn a_save_targets_the_users_database_and_the_folders_id() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let root = CardRoot::for_user(&dir, None)?;
        let folder = root.path().join("Spanish");
        std::fs::create_dir_all(&folder)?;
        std::fs::write(folder.join("verbs.md"), "Q: a\nA: b\n")?;
        let id = crate::cmd::serve::cards::collection_id(&folder)?;

        let db_dir = dir.join("db");
        let (path, found) = db_target_for(
            &root,
            &collection_folder(&root, "Spanish/verbs.md")?,
            &db_dir,
        )?;
        assert_eq!(path, db_dir.join("default.db"));
        assert_eq!(found, id);
        Ok(())
    }

    #[test]
    fn a_file_outside_any_collection_folder_is_rejected() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let root = CardRoot::for_user(&dir, None)?;
        std::fs::write(root.path().join("loose.md"), "Q: a\nA: b\n")?;
        assert!(collection_folder(&root, "loose.md").is_err());
        Ok(())
    }

    #[test]
    fn parse_buffer_names_the_deck_after_the_file() -> Fallible<()> {
        let parsed = parse_buffer("Spanish/verbs.md", "Q: the cat\nA: el gato\n")?;
        assert_eq!(parsed.cards.len(), 1);
        assert_eq!(parsed.cards[0].deck_name(), "verbs");
        Ok(())
    }

    #[test]
    fn parse_buffer_honours_toml_frontmatter_offset() -> Fallible<()> {
        let text = "---\nname = \"Custom\"\n---\n\nQ: a\nA: b\n";
        let parsed = parse_buffer("Spanish/verbs.md", text)?;
        assert_eq!(parsed.cards.len(), 1);
        Ok(())
    }

    #[test]
    fn preview_of_valid_markdown_lists_every_card() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let root = CardRoot::for_user(&dir, None)?;
        std::fs::create_dir_all(root.path().join("Spanish"))?;
        let html = crate::cmd::serve::files_ui::render_preview(
            &root,
            "Spanish/verbs.md",
            "Q: the cat\nA: el gato\n\nC: A [dog] barks.",
        )
        .into_string();
        assert!(html.contains("2 cards"), "got: {html}");
        assert!(html.contains("el gato"), "got: {html}");
        Ok(())
    }

    #[test]
    fn preview_of_broken_markdown_reports_the_parse_error() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let root = CardRoot::for_user(&dir, None)?;
        std::fs::create_dir_all(root.path().join("Spanish"))?;
        let html = crate::cmd::serve::files_ui::render_preview(
            &root,
            "Spanish/verbs.md",
            "Q: dangling question with no answer\n",
        )
        .into_string();
        assert!(html.contains("does not parse"), "got: {html}");
        Ok(())
    }

    #[test]
    fn lost_history_is_reported_only_when_cards_were_actually_reviewed() -> Fallible<()> {
        // Replacing the seeded template's cards on a file nobody has drilled
        // must not warn about review history: there was none to lose.
        let dir = create_tmp_directory()?;
        let db = UserDatabase::open(&dir.join("empty.db"))?
            .collection(CollectionId::new("test-collection")?);

        let old = parse_buffer("Spanish/verbs.md", CARD_TEMPLATE)?.cards;

        // A row exists per card once a collection is loaded, but nobody has
        // reviewed them, so there is no history to lose.
        let now = crate::types::timestamp::Timestamp::now();
        for card in &old {
            db.insert_card(card.hash(), now)?;
        }
        assert!(!any_card_has_history(&db, &old)?);

        // Once one is actually reviewed, the warning must fire again.
        db.update_card_performance(
            old[0].hash(),
            Performance::Reviewed(crate::types::performance::ReviewedPerformance {
                last_reviewed_at: now,
                stability: 1.0,
                difficulty: 3.0,
                interval_raw: 1.0,
                interval_days: 1,
                due_date: now.date(),
                review_count: 1,
            }),
        )?;
        assert!(any_card_has_history(&db, &old)?);
        Ok(())
    }

    /// An `AppState` whose card trees live under `data_dir`.
    fn state_for(data_dir: &Path) -> AppState {
        crate::cmd::serve::state::test_support::state_with_data_dir(data_dir.to_path_buf())
    }

    /// Put a saved deck in `state` and return its URL slug.
    ///
    /// A deck slug is the one name a card folder may not take: both are
    /// addressed through `/collection/{slug}` and routing prefers the
    /// collection, so a folder that matched one would make the deck
    /// unreachable.
    fn reserve_deck(state: &AppState, name: &str) -> String {
        use crate::cmd::serve::decks::ResolvedCustomDeck;
        use crate::cmd::serve::decks::slug_for_deck;

        let slug = slug_for_deck(name, None);
        state.custom_decks.lock().push(ResolvedCustomDeck {
            name: name.to_string(),
            slug: slug.clone(),
            owner: None,
            members: Vec::new(),
        });
        slug
    }

    #[test]
    fn a_new_folder_may_not_shadow_a_saved_deck() -> Fallible<()> {
        // Routing prefers collections, so a folder named after a deck would
        // make the deck unreachable while showing up as its own row.
        let dir = create_tmp_directory()?;
        let state = state_for(&dir);
        let taken = reserve_deck(&state, "Exam revision");
        let form = NewEntryForm {
            parent: String::new(),
            name: taken.clone(),
        };

        let error = match create_entry(&state, None, &form, true) {
            Ok(_) => return fail("expected a slug collision error"),
            Err(e) => e.to_string(),
        };
        assert!(error.contains(&taken), "got: {error}");
        assert!(
            !user_root(&state, None)?.path().join(&taken).exists(),
            "the folder must not be created"
        );
        Ok(())
    }

    #[test]
    fn a_new_folder_may_not_shadow_another_local_folder_slug() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let state = state_for(&dir);
        let first = NewEntryForm {
            parent: String::new(),
            name: "Verbs 1".to_string(),
        };
        create_entry(&state, None, &first, true)?;

        let second = NewEntryForm {
            parent: String::new(),
            name: "Verbs-1".to_string(),
        };
        assert!(create_entry(&state, None, &second, true).is_err());
        Ok(())
    }

    #[test]
    fn a_subfolder_may_be_named_after_a_collection() -> Fallible<()> {
        // Only top-level folders are collections, so nothing below the root
        // shares the slug namespace.
        let dir = create_tmp_directory()?;
        let state = state_for(&dir);
        let taken = reserve_deck(&state, "Exam revision");
        create_entry(
            &state,
            None,
            &NewEntryForm {
                parent: String::new(),
                name: "Languages".to_string(),
            },
            true,
        )?;
        create_entry(
            &state,
            None,
            &NewEntryForm {
                parent: "Languages".to_string(),
                name: taken,
            },
            true,
        )?;
        Ok(())
    }

    #[test]
    fn renaming_a_folder_onto_a_taken_slug_is_refused() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let state = state_for(&dir);
        let taken = reserve_deck(&state, "Exam revision");
        create_entry(
            &state,
            None,
            &NewEntryForm {
                parent: String::new(),
                name: "Espanol".to_string(),
            },
            true,
        )?;

        let form = RenameForm {
            path: "Espanol".to_string(),
            name: taken,
        };
        assert!(rename_entry(&state, None, &form).is_err());
        // Renaming a folder to its own name is not a collision with itself.
        let same = RenameForm {
            path: "Espanol".to_string(),
            name: "Espanol".to_string(),
        };
        assert!(rename_entry(&state, None, &same).is_err(), "already exists");
        Ok(())
    }

    /// A save that cannot be completed must leave the file as it was: the
    /// flash says "not saved", and the disk has to agree.
    #[test]
    fn a_save_outside_any_collection_folder_leaves_the_file_alone() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let state = state_for(&dir);
        let root = user_root(&state, None)?;
        let path = root.path().join("loose.md");
        std::fs::write(&path, "Q: a\nA: b\n")?;

        let form = SaveForm {
            content: "Q: replaced\nA: replaced\n".to_string(),
            mtime: file_mtime_ms(&path)?,
        };
        assert!(save_file(&state, None, "loose.md", &form).is_err());
        assert_eq!(std::fs::read_to_string(&path)?, "Q: a\nA: b\n");
        Ok(())
    }

    /// A file that does not parse any more — edited outside the UI, or
    /// restored from elsewhere — must still be repairable in the editor.
    #[test]
    fn a_file_whose_content_does_not_parse_can_still_be_repaired() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let state = state_for(&dir);
        let root = user_root(&state, None)?;
        std::fs::create_dir_all(root.path().join("Spanish"))?;
        let path = root.path().join("Spanish").join("verbs.md");
        std::fs::write(&path, "Q: a question with no answer\n")?;

        let fixed = "Q: the cat\nA: el gato\n";
        let form = SaveForm {
            content: fixed.to_string(),
            mtime: file_mtime_ms(&path)?,
        };
        save_file(&state, None, "Spanish/verbs.md", &form)?;
        assert_eq!(std::fs::read_to_string(&path)?, fixed);
        Ok(())
    }

    #[test]
    fn deleting_a_collection_folder_removes_its_review_database() -> Fallible<()> {
        // Otherwise `{id}.db` is orphaned in `db/` while a folder recreated
        // under the same name silently starts its history over.
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
        let db_path = dir.join("db").join(format!("{id}.db"));
        std::fs::create_dir_all(dir.join("db"))?;
        std::fs::write(&db_path, "")?;

        delete_entry(
            &state,
            None,
            &DeleteForm {
                path: "Spanish".to_string(),
            },
        )?;
        assert!(!root.path().join("Spanish").exists());
        assert!(!db_path.exists(), "the review database is orphaned");
        Ok(())
    }

    /// A save while a session is drilling the same collection rewrites the
    /// card hashes the session is still grading against, stranding its
    /// review history. `edit_post_inner` refuses a card edit for exactly
    /// this reason; the whole-file editor has to refuse it too.
    /// A session keyed by `slug`, exactly as a drill on
    /// `/collection/{slug}` would leave one.
    fn start_session(state: &AppState, data_dir: &Path, folder: &Path, slug: &str) -> Fallible<()> {
        use crate::cmd::drill::render::AnswerControls;
        use crate::cmd::drill::state::MutableState;
        use crate::cmd::drill::state::SessionDbs;
        use crate::cmd::serve::state::DrillSession;
        use crate::cmd::serve::state::SessionKey;
        use crate::rng::TinyRng;
        use crate::types::performance::Jitter;
        use crate::types::performance::Scheduling;

        let db_dir = data_dir.join("db");
        ensure_dir(&db_dir, "review database directory")?;
        let root = CardRoot::for_user(data_dir, None)?;
        let (db_path, id) = db_target_for(&root, folder, &db_dir)?;
        let started_at = Timestamp::now();
        let db = UserDatabase::open(&db_path)?.collection(id);
        let session_id = db.create_session(started_at)?;
        let mutable = MutableState::new(
            SessionDbs::single(
                db,
                session_id,
                Scheduling {
                    jitter: Jitter::none(),
                    ..Scheduling::default()
                },
            ),
            crate::cmd::drill::cache::Cache::new(),
            Vec::new(),
            TinyRng::from_seed(1),
        );
        let session = std::sync::Arc::new(parking_lot::Mutex::new(DrillSession::new(
            folder.to_path_buf(),
            Vec::new(),
            started_at,
            AnswerControls::Full,
            mutable,
        )));
        state
            .sessions
            .lock()
            .insert(SessionKey::new(None, slug), session);
        Ok(())
    }

    /// Start a session keyed by `deck_slug` that draws its cards from
    /// `folder` — the shape a custom-deck drill has. `SessionDb.source`
    /// carries the collection, which is the only place that fact is
    /// recorded: the sessions map is keyed by the deck's own slug.
    fn start_deck_session(
        state: &AppState,
        data_dir: &Path,
        folder: &Path,
        deck_slug: &str,
    ) -> Fallible<()> {
        use crate::cmd::drill::render::AnswerControls;
        use crate::cmd::drill::state::MutableState;
        use crate::cmd::drill::state::SessionDb;
        use crate::cmd::drill::state::SessionDbs;
        use crate::cmd::drill::state::SessionSource;
        use crate::cmd::serve::state::DrillSession;
        use crate::cmd::serve::state::SessionKey;
        use crate::rng::TinyRng;
        use crate::types::performance::Scheduling;

        let db_dir = data_dir.join("db");
        ensure_dir(&db_dir, "review database directory")?;
        let root = CardRoot::for_user(data_dir, None)?;
        let (db_path, id) = db_target_for(&root, folder, &db_dir)?;
        let started_at = Timestamp::now();
        let db = UserDatabase::open(&db_path)?.collection(id);
        let session_id = db.create_session(started_at)?;
        let dbs = SessionDbs::routed(
            vec![SessionDb {
                db,
                session_id,
                source: Some(SessionSource {
                    coll_dir: folder.to_path_buf(),
                    file_url_prefix: format!("/collection/{deck_slug}/file"),
                }),
                scheduling: Scheduling::default(),
            }],
            std::collections::HashMap::new(),
        );
        let mutable = MutableState::new(
            dbs,
            crate::cmd::drill::cache::Cache::new(),
            Vec::new(),
            TinyRng::from_seed(1),
        );
        let session = std::sync::Arc::new(parking_lot::Mutex::new(DrillSession::new(
            // A deck session's own directory is not any one collection's.
            data_dir.to_path_buf(),
            Vec::new(),
            started_at,
            AnswerControls::Full,
            mutable,
        )));
        state
            .sessions
            .lock()
            .insert(SessionKey::new(None, deck_slug), session);
        Ok(())
    }

    /// A custom-deck session is keyed by the deck's slug, not the
    /// collection's, so the guard that looked the collection slug up in the
    /// sessions map never saw it: deleting the collection unlinked the
    /// database that session still held open.
    #[test]
    fn a_delete_is_refused_while_a_custom_deck_session_uses_the_collection() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let state = state_for(&dir);
        let root = user_root(&state, None)?;
        // Emptied of cards, so a folder that still holds files is not what
        // refuses the delete: only the running session can.
        let folder = root.path().join("Spanish");
        std::fs::create_dir_all(&folder)?;
        collection_id(&folder)?;

        start_deck_session(&state, &dir, &folder, "deck-exam-0badc0de")?;

        assert!(
            delete_entry(
                &state,
                None,
                &DeleteForm {
                    path: "Spanish".to_string(),
                },
            )
            .is_err(),
            "a deck session drawing on this collection must block the delete"
        );
        assert!(folder.exists());
        Ok(())
    }

    /// Same bug, same shape, on the rename path.
    #[test]
    fn a_rename_is_refused_while_a_custom_deck_session_uses_the_collection() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let state = state_for(&dir);
        let root = user_root(&state, None)?;
        let folder = root.path().join("Spanish");
        std::fs::create_dir_all(&folder)?;
        std::fs::write(folder.join("verbs.md"), "Q: the cat\nA: el gato\n")?;

        start_deck_session(&state, &dir, &folder, "deck-exam-0badc0de")?;

        assert!(
            rename_entry(
                &state,
                None,
                &RenameForm {
                    path: "Spanish".to_string(),
                    name: "Espanol".to_string(),
                },
            )
            .is_err(),
            "a deck session drawing on this collection must block the rename"
        );
        assert!(folder.exists());
        Ok(())
    }

    /// The whole-file editor refused to save while a session was running,
    /// for the same reason the card editor did. It now re-keys the session
    /// onto the rewritten file instead.
    #[test]
    fn a_save_migrates_a_running_session_instead_of_refusing() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let state = state_for(&dir);
        let root = user_root(&state, None)?;
        let folder = root.path().join("Spanish");
        std::fs::create_dir_all(&folder)?;
        let path = folder.join("verbs.md");
        std::fs::write(&path, "Q: the cat\nA: el gato\n")?;

        start_session(&state, &dir, &folder, "Spanish")?;

        let saved = save_file(
            &state,
            None,
            "Spanish/verbs.md",
            &SaveForm {
                content: "Q: the cat\nA: el gato (masc.)\n".to_string(),
                mtime: file_mtime_ms(&path)?,
            },
        )?;
        assert!(saved.starts_with("Saved"), "got: {saved}");
        assert!(
            std::fs::read_to_string(&path)?.contains("masc."),
            "the edit is not on disk"
        );
        Ok(())
    }

    /// `Collection::open` validates media when it loads, so an
    /// image reference to a file that is not there does not render as a
    /// broken image — it fails the whole collection, and its page, deck
    /// tree, stats and landing count all go with it. The editor must not be
    /// a way to get there.
    #[test]
    fn a_save_naming_an_image_that_is_not_there_is_refused() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let state = state_for(&dir);
        let root = user_root(&state, None)?;
        let folder = root.path().join("Spanish");
        std::fs::create_dir_all(&folder)?;
        let path = folder.join("verbs.md");
        let original = "Q: the cat\nA: el gato\n";
        std::fs::write(&path, original)?;

        let form = SaveForm {
            content: "Q: the cat\nA: ![](nope.png)\n".to_string(),
            mtime: file_mtime_ms(&path)?,
        };
        assert!(save_file(&state, None, "Spanish/verbs.md", &form).is_err());
        assert_eq!(
            std::fs::read_to_string(&path)?,
            original,
            "a refused save must leave the file as it was"
        );

        // The same reference, once the image is actually there, saves.
        std::fs::write(folder.join("nope.png"), "x")?;
        let form = SaveForm {
            content: "Q: the cat\nA: ![](nope.png)\n".to_string(),
            mtime: file_mtime_ms(&path)?,
        };
        save_file(&state, None, "Spanish/verbs.md", &form)?;
        Ok(())
    }

    /// A refused save used to redirect to the editor's GET, which re-reads
    /// the file the refusal had already put back — so one bad card threw
    /// away everything the user had typed. The response must carry the
    /// submitted buffer instead.
    #[tokio::test]
    async fn a_refused_save_answers_with_the_buffer_that_was_submitted() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let state = state_for(&dir);
        let root = user_root(&state, None)?;
        std::fs::create_dir_all(root.path().join("Spanish"))?;
        let path = root.path().join("Spanish").join("verbs.md");
        std::fs::write(&path, "Q: the cat\nA: el gato\n")?;

        let typed = "Q: hours of work\nA: el gato\n\nQ: a dangling question\n";
        let response = editor_post_handler(
            State(state),
            AxumPath("Spanish/verbs.md".to_string()),
            None,
            Form(SaveForm {
                content: typed.to_string(),
                mtime: file_mtime_ms(&path)?,
            }),
        )
        .await;
        let (status, html) = match response {
            Ok(_) => return fail("expected an unparseable buffer to be refused"),
            Err((status, Html(html))) => (status, html),
        };
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(html.contains("hours of work"), "the buffer is gone: {html}");
        // And the mtime is the one on disk now, so the next save is not
        // refused a second time as a phantom conflict.
        assert!(
            html.contains(&format!("value=\"{}\"", file_mtime_ms(&path)?)),
            "got: {html}"
        );
        Ok(())
    }

    /// A file created under a parent that does not exist yet brings its
    /// top-level folder into being, and that folder is a collection. One
    /// whose slug is taken is dropped from the listing with only a log line
    /// to say why, so the check cannot live on the root form alone.
    #[test]
    fn creating_a_file_may_not_conjure_a_shadowing_collection() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let state = state_for(&dir);
        let taken = reserve_deck(&state, "Exam revision");

        let form = NewEntryForm {
            parent: taken.clone(),
            name: "verbs".to_string(),
        };
        assert!(create_entry(&state, None, &form, false).is_err());
        assert!(!user_root(&state, None)?.path().join(&taken).exists());

        // Nested folders are checked by their top-level ancestor too.
        let nested = NewEntryForm {
            parent: format!("{taken}/Unit 2"),
            name: "verbs".to_string(),
        };
        assert!(create_entry(&state, None, &nested, false).is_err());
        Ok(())
    }

    /// A file created deeper down gives its new top-level folder an id, so
    /// the collection it becomes keeps its database across a rename.
    #[test]
    fn a_file_creates_its_collection_with_an_id() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let state = state_for(&dir);
        create_entry(
            &state,
            None,
            &NewEntryForm {
                parent: "Spanish/Unit 2".to_string(),
                name: "verbs".to_string(),
            },
            false,
        )?;
        let root = user_root(&state, None)?;
        assert!(root.path().join("Spanish/Unit 2/verbs.md").is_file());
        assert!(existing_collection_id(&root.path().join("Spanish"))?.is_some());
        Ok(())
    }

    /// The editor cannot save a file outside a collection and cannot serve
    /// its images, so it refuses to open one rather than presenting a page
    /// that does not work.
    #[test]
    fn the_editor_refuses_a_file_outside_any_collection() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let state = state_for(&dir);
        let root = user_root(&state, None)?;
        std::fs::write(root.path().join("loose.md"), "Q: a\nA: b\n")?;
        assert!(load_for_edit(&state, None, "loose.md").is_err());
        Ok(())
    }

    /// The database is removed only once the folder actually is. Removing
    /// it first threw the review history away whenever `remove_dir_all`
    /// failed — leaving the collection on disk, silently starting over.
    ///
    /// Unix only: the failure is staged by making the parent directory
    /// read-only, which Windows permissions do not express — the mode bits
    /// this needs are not in `std` there, so the test would not even
    /// compile.
    #[cfg(unix)]
    #[test]
    fn a_failed_folder_removal_keeps_the_review_database() -> Fallible<()> {
        use std::os::unix::fs::PermissionsExt;

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
        let folder = root.path().join("Spanish");
        // The file `remove_collection_database` acts on: the collection's
        // own legacy database, not the user's.
        let db_path = dir
            .join("db")
            .join(format!("{}.db", collection_id(&folder)?));
        ensure_dir(&dir.join("db"), "review database directory")?;
        std::fs::write(&db_path, "")?;

        // A read-only parent makes unlinking the folder fail while leaving
        // everything readable, which is exactly the shape of the failure
        // (permissions, a file held open) this ordering is for.
        let original = std::fs::metadata(root.path())?.permissions();
        let mut locked = original.clone();
        locked.set_mode(0o555);
        std::fs::set_permissions(root.path(), locked)?;
        let outcome = delete_entry(
            &state,
            None,
            &DeleteForm {
                path: "Spanish".to_string(),
            },
        );
        std::fs::set_permissions(root.path(), original)?;

        // Running as root defeats the permission bits; the ordering is only
        // observable when the removal really did fail.
        if outcome.is_err() {
            assert!(folder.exists(), "the folder survived, as set up");
            assert!(
                db_path.exists(),
                "the folder is still there but its review history is gone"
            );
        }
        Ok(())
    }

    /// The media folder is hashcards' storage for pasted images, addressed
    /// only through the cards that reference it — listing it would invite a
    /// rename that breaks every one of them.
    #[test]
    fn the_media_folder_is_hidden_inside_a_collection() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let root = CardRoot::for_user(&dir, None)?;
        std::fs::create_dir_all(root.path().join("Spanish").join("media"))?;
        std::fs::write(root.path().join("Spanish").join("media").join("a.png"), "x")?;
        std::fs::write(root.path().join("Spanish").join("verbs.md"), "Q: a\nA: b\n")?;
        // A collection the user chose to call `media` is their own folder,
        // not ours: only the one inside a collection is bookkeeping.
        std::fs::create_dir_all(root.path().join("media"))?;

        let paths: Vec<String> = read_tree(&root)?.into_iter().map(|e| e.rel_path).collect();
        assert_eq!(paths, vec!["Spanish", "Spanish/verbs.md", "media"]);
        Ok(())
    }

    /// `read_tree` hides a `media` folder inside a collection and
    /// `non_empty_children` does not count it, both because it is ours. A
    /// folder of the user's own under that name would be invisible to the
    /// tree *and* to the "refuse a non-empty folder" guard, so deleting the
    /// collection would silently take the decks inside it — and their review
    /// history — with it.
    #[test]
    fn a_user_folder_inside_a_collection_may_not_be_called_media() -> Fallible<()> {
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

        let form = NewEntryForm {
            parent: "Spanish".to_string(),
            name: MEDIA_DIR.to_string(),
        };
        let error = match create_entry(&state, None, &form, true) {
            Ok(_) => return fail("expected `media` to be refused inside a collection"),
            Err(e) => e.to_string(),
        };
        assert!(error.contains("pasted images"), "got: {error}");

        // Renaming onto the same name is the same hazard.
        create_entry(
            &state,
            None,
            &NewEntryForm {
                parent: "Spanish".to_string(),
                name: "Unit 2".to_string(),
            },
            true,
        )?;
        assert!(
            rename_entry(
                &state,
                None,
                &RenameForm {
                    path: "Spanish/Unit 2".to_string(),
                    name: MEDIA_DIR.to_string(),
                },
            )
            .is_err()
        );

        // A deck called `media.md` is not a folder and stays allowed, and so
        // does a collection of the user's own called `media`.
        create_entry(
            &state,
            None,
            &NewEntryForm {
                parent: "Spanish".to_string(),
                name: MEDIA_DIR.to_string(),
            },
            false,
        )?;
        create_entry(
            &state,
            None,
            &NewEntryForm {
                parent: String::new(),
                name: MEDIA_DIR.to_string(),
            },
            true,
        )?;
        Ok(())
    }

    /// The name is reserved now, but a `media` folder made by hand before
    /// that is still on disk, holding decks nothing lists and nothing
    /// counts. Deleting the collection used to take them silently.
    #[test]
    fn a_collection_whose_media_folder_holds_decks_is_not_deleted() -> Fallible<()> {
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
        let folder = user_root(&state, None)?.path().join("Spanish");
        let hidden = folder.join(MEDIA_DIR).join("Unit 2");
        std::fs::create_dir_all(&hidden)?;
        std::fs::write(hidden.join("verbs.md"), "Q: the cat\nA: el gato\n")?;

        let error = match delete_entry(
            &state,
            None,
            &DeleteForm {
                path: "Spanish".to_string(),
            },
        ) {
            Ok(_) => return fail("expected hidden card files to refuse the delete"),
            Err(e) => e.to_string(),
        };
        assert!(error.contains("card files"), "got: {error}");
        assert!(hidden.join("verbs.md").exists());
        Ok(())
    }

    /// Deleting a collection unlinks its database. A session still holding
    /// that file open writes its remaining grades to an inode nothing can
    /// read back, so the change has to be refused while one is running —
    /// the same guard `save_file` applies to an edit.
    #[test]
    fn a_collection_is_not_deleted_or_renamed_while_a_session_drills_it() -> Fallible<()> {
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
        let folder = root.path().join("Spanish");
        let path = folder.join("verbs.md");
        std::fs::write(&path, "Q: the cat\nA: el gato\n")?;
        start_session(&state, &dir, &folder, "Spanish")?;

        // The deck inside it, first: deleting that is what empties the
        // folder out for the delete below.
        let error = match delete_entry(
            &state,
            None,
            &DeleteForm {
                path: "Spanish/verbs.md".to_string(),
            },
        ) {
            Ok(_) => return fail("expected an active session to refuse the delete"),
            Err(e) => e.to_string(),
        };
        assert!(error.contains("drill session"), "got: {error}");
        assert!(path.exists());

        assert!(
            delete_entry(
                &state,
                None,
                &DeleteForm {
                    path: "Spanish".to_string(),
                },
            )
            .is_err()
        );
        assert!(
            rename_entry(
                &state,
                None,
                &RenameForm {
                    path: "Spanish".to_string(),
                    name: "Espanol".to_string(),
                },
            )
            .is_err()
        );
        assert!(folder.exists());

        // Once the session is over, both go through.
        state.sessions.lock().clear();
        delete_entry(
            &state,
            None,
            &DeleteForm {
                path: "Spanish/verbs.md".to_string(),
            },
        )?;
        Ok(())
    }

    /// A collection is a top-level folder, so a file directly in the root
    /// has no review database and no way to serve its images. Creating one
    /// used to write the file and *then* report `Not a directory (os error
    /// 20)`, from giving a regular file a collection id.
    #[test]
    fn a_card_file_cannot_be_created_directly_in_the_root() -> Fallible<()> {
        let dir = create_tmp_directory()?;
        let state = state_for(&dir);
        let form = NewEntryForm {
            parent: String::new(),
            name: "notes".to_string(),
        };
        let error = match create_entry(&state, None, &form, false) {
            Ok(_) => return fail("expected a root-level file to be refused"),
            Err(e) => e.to_string(),
        };
        assert!(error.contains("collection folder"), "got: {error}");
        assert!(
            !user_root(&state, None)?.path().join("notes.md").exists(),
            "the file must not be left behind"
        );
        Ok(())
    }

    /// `CardRoot::resolve` normalizes a path while the collection checks
    /// used to split the raw one, so `./Spanish` was deleted as if it were
    /// nested: no id read, and `{id}.db` left orphaned in `db/`.
    #[test]
    fn a_dotted_path_is_still_recognized_as_a_collection_root() -> Fallible<()> {
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
        let folder = root.path().join("Spanish");
        // The file `remove_collection_database` acts on: the collection's
        // own legacy database, not the user's.
        let db_path = dir
            .join("db")
            .join(format!("{}.db", collection_id(&folder)?));
        ensure_dir(&dir.join("db"), "review database directory")?;
        std::fs::write(&db_path, "")?;

        // The same dotted path must also find the session keyed by `Spanish`.
        start_session(&state, &dir, &folder, "Spanish")?;
        assert!(
            delete_entry(
                &state,
                None,
                &DeleteForm {
                    path: "./Spanish".to_string(),
                },
            )
            .is_err(),
            "a dotted path must not slip past the active-session guard"
        );
        state.sessions.lock().clear();

        delete_entry(
            &state,
            None,
            &DeleteForm {
                path: "./Spanish".to_string(),
            },
        )?;
        assert!(!folder.exists());
        assert!(!db_path.exists(), "the review database is orphaned");
        Ok(())
    }

    #[test]
    fn a_collection_holding_only_pasted_images_can_still_be_deleted() -> Fallible<()> {
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
        let folder = user_root(&state, None)?.path().join("Spanish");
        std::fs::create_dir_all(folder.join("media"))?;
        std::fs::write(folder.join("media").join("a.png"), "x")?;

        delete_entry(
            &state,
            None,
            &DeleteForm {
                path: "Spanish".to_string(),
            },
        )?;
        assert!(!folder.exists());
        Ok(())
    }
}
