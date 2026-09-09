use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

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

use crate::cmd::drill::state::CardMigration;
use crate::cmd::drill::state::MigrationEffect;
use crate::cmd::drill::template::page_template;
use crate::cmd::run_blocking;
use crate::cmd::serve::auth::CurrentUser;
use crate::cmd::serve::config::ResolvedCollection;
use crate::cmd::serve::handlers::DrillTarget;
use crate::cmd::serve::handlers::deck_sources;
use crate::cmd::serve::handlers::find_drill_target;
use crate::cmd::serve::reviewdb::open_collection_db;
use crate::cmd::serve::state::AppState;
use crate::cmd::serve::state::migrate_sessions;
use crate::error::ErrorReport;
use crate::error::Fallible;
use crate::error::fail;
use crate::flash::Flash;
use crate::parser::Parser;
use crate::parser::parse_deck;
use crate::parser::strip_frontmatter_with_offset;
use crate::types::card::Card;
use crate::types::card::CardContent;
use crate::types::card_hash::CardHash;
use crate::types::timestamp::Timestamp;

/// Where a card edit returns to when it is saved.
///
/// Two values, not a caller-supplied path: a redirect target taken from a
/// query string or a form field is an open redirect, and these cover every
/// entry point the editor has. `/collection/{slug}` renders the drill when
/// a session is live and the topic browser when it is not, so the pencil in
/// a session and the edit link on the topic tree can share one value.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReturnTo {
    Bookmarks,
    Collection,
}

impl ReturnTo {
    /// Anything unrecognized is the bookmarks page. A bad value in a URL is
    /// not worth an error page: the user still gets somewhere sensible.
    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("collection") => ReturnTo::Collection,
            _ => ReturnTo::Bookmarks,
        }
    }

    pub fn target(self, slug: &str) -> String {
        match self {
            ReturnTo::Collection => format!("/collection/{slug}"),
            ReturnTo::Bookmarks => format!("/collection/{slug}/bookmarks"),
        }
    }

    /// The spelling that round-trips through the form's hidden field.
    pub fn as_str(self) -> &'static str {
        match self {
            ReturnTo::Collection => "collection",
            ReturnTo::Bookmarks => "bookmarks",
        }
    }
}

// ── GET handler ───────────────────────────────────────────────────────────────

pub async fn edit_get_handler(
    State(state): State<AppState>,
    AxumPath((slug, hash_hex)): AxumPath<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    current_user: Option<CurrentUser>,
) -> (StatusCode, Html<String>) {
    let owner = current_user.map(|u| u.email);
    let return_to = ReturnTo::parse(query.get("return_to").map(String::as_str));
    let state2 = state.clone();
    let slug2 = slug.clone();
    let hash2 = hash_hex.clone();
    match run_blocking(move || edit_get_inner(&state2, &slug2, &hash2, owner.as_deref(), return_to))
        .await
    {
        Ok(html) => (StatusCode::OK, Html(html)),
        Err(e) => error_page(&slug, &hash_hex, &e.to_string()),
    }
}

fn edit_get_inner(
    state: &AppState,
    slug: &str,
    hash_hex: &str,
    owner: Option<&str>,
    return_to: ReturnTo,
) -> Fallible<String> {
    let hash = CardHash::from_hex(hash_hex)?;
    let site = card_site(state, slug, hash, owner)?;
    let card = find_card_by_hash(&site.cards, hash)?;

    let file_path = card.file_path().clone();
    let file_content = std::fs::read_to_string(&file_path)?;
    let mtime_ms = file_mtime_ms(&file_path)?;
    let block = extract_card_block(&file_content, card.range())?;
    let rel_path = file_path
        .strip_prefix(&site.coll_dir)
        .unwrap_or(&file_path)
        .display()
        .to_string();

    let html = render_edit_form(
        &site.target_name,
        slug,
        hash_hex,
        &rel_path,
        &block,
        mtime_ms,
        return_to,
    );
    Ok(html.into_string())
}

/// Where a card being edited actually lives, reached through the slug the
/// caller came from.
struct CardSite {
    /// The collection holding the card's file and its review database.
    rc: ResolvedCollection,
    /// `rc.coll_dir`, canonicalized: card file paths are canonical, so the
    /// relative path shown in the form is taken against this.
    coll_dir: PathBuf,
    /// Every card in that collection, parsed once for both the lookup and
    /// the sibling scan the save needs.
    cards: Vec<Card>,
    /// What the back link is labelled with — the deck's name when the
    /// caller came from a deck, since that is where the link returns to.
    target_name: String,
}

/// Resolve `slug` to the collection holding `hash`.
///
/// The slug is whatever the card was reached under, which is not always a
/// collection: the pencil in a drill links to the slug the *session* is
/// filed under, and for a custom deck that is the deck's own slug, naming
/// no collection at all. A deck's members are searched for the card, which
/// is also the only way a multi-collection deck could name the right one —
/// the card's home need not be the collection the deck is listed under.
fn card_site(
    state: &AppState,
    slug: &str,
    hash: CardHash,
    owner: Option<&str>,
) -> Fallible<CardSite> {
    let target = find_drill_target(state, slug, owner)
        .ok_or_else(|| ErrorReport::new(format!("Unknown collection: {slug}")))?;
    match target {
        DrillTarget::Collection(rc) => {
            let coll_dir = rc.coll_dir.canonicalize()?;
            let cards = parse_deck(&coll_dir)?.cards;
            let target_name = rc.name.clone();
            Ok(CardSite {
                rc,
                coll_dir,
                cards,
                target_name,
            })
        }
        DrillTarget::Deck(deck) => {
            for source in deck_sources(state, &deck, owner) {
                let rc = source.collection;
                let coll_dir = rc.coll_dir.canonicalize()?;
                let cards = parse_deck(&coll_dir)?.cards;
                if cards.iter().any(|c| c.hash() == hash) {
                    return Ok(CardSite {
                        rc,
                        coll_dir,
                        cards,
                        target_name: deck.name.clone(),
                    });
                }
            }
            fail(format!(
                "That card is not in any collection the deck '{}' draws on.",
                deck.name
            ))
        }
    }
}

fn render_edit_form(
    return_label: &str,
    slug: &str,
    hash_hex: &str,
    rel_path: &str,
    block: &str,
    mtime_ms: u64,
    return_to: ReturnTo,
) -> Markup {
    page_template(html! {
        div.edit-page {
            div.browse-header {
                a.back-link href=(return_to.target(slug)) {
                    @match return_to {
                        ReturnTo::Collection => { "\u{2190} " (return_label) }
                        ReturnTo::Bookmarks => { "\u{2190} " (return_label) " Bookmarks" }
                    }
                }
                h1 { "Edit Card" }
            }
            p.edit-path { code { (rel_path) } }
            form action=(format!("/collection/{slug}/edit/{hash_hex}")) method="post" {
                input type="hidden" name="mtime_ms" value=(mtime_ms);
                input type="hidden" name="return_to" value=(return_to.as_str());
                textarea
                    name="new_text"
                    class="edit-textarea"
                    rows="20"
                    spellcheck="false"
                    autocorrect="off"
                    autocapitalize="off"
                {
                    (block)
                }
                div.edit-controls {
                    button type="submit" class="btn btn-primary" { "Save" }
                    " "
                    a.btn.btn-secondary href=(return_to.target(slug)) { "Cancel" }
                }
            }
        }
    })
}

// ── POST handler ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct EditForm {
    pub new_text: String,
    pub mtime_ms: String,
    #[serde(default)]
    pub return_to: Option<String>,
}

/// What a successful edit did, for user-facing reporting.
pub struct EditOutcome {
    /// Cards whose review history was migrated to a new hash.
    pub migrated: usize,
    /// New cards that could not be matched to prior history and start fresh.
    pub skipped: usize,
    /// What the edit did to any live drill session on this collection.
    pub session: MigrationEffect,
}

pub async fn edit_post_handler(
    State(state): State<AppState>,
    AxumPath((slug, hash_hex)): AxumPath<(String, String)>,
    current_user: Option<CurrentUser>,
    Form(form): Form<EditForm>,
) -> Result<Redirect, (StatusCode, Html<String>)> {
    let state2 = state.clone();
    let slug2 = slug.clone();
    let hash2 = hash_hex.clone();
    let owner = current_user.map(|u| u.email);
    let return_to = ReturnTo::parse(form.return_to.as_deref());
    match run_blocking(move || edit_post_inner(&state2, &slug2, &hash2, form, owner.as_deref()))
        .await
    {
        Ok(outcome) => {
            log::debug!(
                "Edit saved: {} card(s) migrated, {} skipped, {} session card(s) renamed, {} dropped",
                outcome.migrated,
                outcome.skipped,
                outcome.session.renamed,
                outcome.session.dropped
            );
            let target = return_to.target(&slug);
            let mut msg = String::from("Card saved.");
            if outcome.session.renamed > 0 || outcome.session.dropped > 0 {
                msg.push_str(" Your running session was updated.");
            }
            if outcome.session.session_finished {
                msg.push_str(" It has no cards left, so it is finished.");
            }
            let flash = if outcome.skipped > 0 {
                Flash::error(format!(
                    "{msg} {} card(s) could not be matched to their previous review history and will start fresh.",
                    outcome.skipped
                ))
            } else {
                Flash::success(msg)
            };
            Ok(flash.redirect(&target))
        }
        Err(e) => Err(error_page(&slug, &hash_hex, &e.to_string())),
    }
}

fn edit_post_inner(
    state: &AppState,
    slug: &str,
    hash_hex: &str,
    form: EditForm,
    owner: Option<&str>,
) -> Fallible<EditOutcome> {
    let hash = CardHash::from_hex(hash_hex)?;
    let CardSite {
        rc,
        coll_dir,
        cards,
        ..
    } = card_site(state, slug, hash, owner)?;
    let card = find_card_by_hash(&cards, hash)?;

    let file_path = card.file_path().clone();
    let current_mtime = file_mtime_ms(&file_path)?;
    let submitted_mtime: u64 = form
        .mtime_ms
        .parse()
        .map_err(|_| ErrorReport::new("Invalid mtime in form"))?;
    if current_mtime != submitted_mtime {
        return fail(
            "The source file changed on disk since you opened this form. Reload and try again.",
        );
    }

    // Collect sibling cards at the same (file_path, range) (cloze siblings share a range).
    let range = card.range();
    let old_cards: Vec<&Card> = cards
        .iter()
        .filter(|c| c.file_path() == &file_path && c.range() == range)
        .collect();

    let new_text = form.new_text.trim().to_string();
    let file_content = std::fs::read_to_string(&file_path)?;

    splice_card_block(&file_path, &file_content, range, &new_text, submitted_mtime)?;

    // Re-parse the modified file; revert on any parse error.
    let new_file_content = std::fs::read_to_string(&file_path)?;
    let (after_fm, line_offset) = strip_frontmatter_with_offset(&new_file_content)?;
    let new_cards_result =
        Parser::new(card.deck_name().clone(), file_path.clone(), line_offset).parse(after_fm);

    let new_cards = match new_cards_result {
        Ok(c) => c,
        Err(e) => {
            revert_file(&file_path, &file_content)?;
            return fail(format!("Parse error — edit reverted: {e}"));
        }
    };

    // The splice replaced the old block with `new_text`, so the new block
    // occupies exactly `new_text.lines().count()` lines starting at the same
    // absolute line. Matching on the exact start line instead would miss the
    // card whenever the edit adds a line above it (a heading, a blank line),
    // planning no rename at all and silently orphaning its review history.
    let new_block_end = range.0 + new_text.lines().count();
    let new_at_block: Vec<&Card> = new_cards
        .iter()
        .filter(|c| {
            c.file_path() == &file_path && c.range().0 >= range.0 && c.range().0 < new_block_end
        })
        .collect();

    let plan = plan_hash_migration(&old_cards, &new_at_block);

    let db = open_collection_db(state, &rc)?;
    let now = Timestamp::now();

    // One transaction, so a failure part-way through cannot leave the
    // database half-migrated behind an already-rewritten file. If it fails
    // anyway, put the file back the way it was.
    let counts = match db.apply_edit_migration(&plan.renames, &plan.fresh, now) {
        Ok(counts) => counts,
        Err(e) => {
            revert_file(&file_path, &file_content)?;
            return fail(format!(
                "Edit reverted: the review history could not be updated: {e}"
            ));
        }
    };

    // The file is written and the transaction has committed, so nothing
    // below can fail: a live session is re-keyed onto the cards that now
    // exist, or it is not, and there is no state left to roll back to.
    let migration = build_card_migration(&plan, &old_cards, &new_cards);
    let session = migrate_sessions(state, &coll_dir, &migration);

    Ok(EditOutcome {
        migrated: counts.renamed,
        // A rename the database declined as a true collision (its target
        // hash already has history) leaves the old row unmatched, which the
        // user should hear about. A rename whose old hash had no history of
        // its own is not: there was nothing to lose.
        skipped: plan.skipped + counts.collided,
        session,
    })
}

// ── Core splice logic ─────────────────────────────────────────────────────────

fn is_card_terminator(line: &str) -> bool {
    line.starts_with("Q:") || line.starts_with("C:") || line.trim() == "---"
}

/// Compute the exclusive upper bound for splicing a card block.
///
/// `Card.range().1` is the terminator line index in non-EOF cases (exclusive)
/// and the last content line index in the EOF case (inclusive). This function
/// normalises to an exclusive bound suitable for `lines[start..end]`.
fn block_end(lines: &[&str], range: (usize, usize)) -> usize {
    let end = range.1;
    // `end > range.0` guards the one-line card case: a single-line `C:`/`Q:`
    // card's own first line is not the terminator of its own block.
    if end > range.0 && end < lines.len() && is_card_terminator(lines[end]) {
        end
    } else {
        (end + 1).min(lines.len())
    }
}

/// Extract the raw markdown source block for a card (the lines the user edits).
/// `range` uses absolute 0-based file line numbers.
pub fn extract_card_block(file_content: &str, range: (usize, usize)) -> Fallible<String> {
    let lines: Vec<&str> = file_content.lines().collect();
    let start = range.0;
    let end = block_end(&lines, range);
    if start > lines.len() {
        return fail("Card range is out of bounds in source file");
    }
    Ok(lines[start..end].join("\n"))
}

/// Atomically replace a card's block in the source file with `new_text`.
///
/// `range` uses absolute 0-based file line numbers. `expected_mtime_ms` is the
/// modification time the caller last observed; it is re-checked immediately
/// before the rename so a concurrent change to the file is never silently
/// overwritten.
fn splice_card_block(
    file_path: &Path,
    file_content: &str,
    range: (usize, usize),
    new_text: &str,
    expected_mtime_ms: u64,
) -> Fallible<()> {
    let ends_with_newline = file_content.ends_with('\n');
    let mut lines: Vec<&str> = file_content.lines().collect();
    let start = range.0;
    let end = block_end(&lines, range);
    let new_lines: Vec<&str> = new_text.lines().collect();
    lines.splice(start..end, new_lines.iter().copied());

    let mut result = lines.join("\n");
    if ends_with_newline {
        result.push('\n');
    }

    let tmp = tmp_path_for(file_path);
    std::fs::write(&tmp, &result)?;
    // TOCTOU guard: the caller checked the mtime long before this point
    // (a full collection re-parse happens in between). Re-check right
    // before the rename.
    if file_mtime_ms(file_path)? != expected_mtime_ms {
        if let Err(e) = std::fs::remove_file(&tmp) {
            return fail(format!(
                "The source file changed on disk since you opened this form, and the temporary file {} could not be removed: {e}. Remove it by hand, then reload and try again.",
                tmp.display()
            ));
        }
        return fail(
            "The source file changed on disk since you opened this form. Reload and try again.",
        );
    }
    std::fs::rename(&tmp, file_path)?;
    Ok(())
}

/// The temporary-file path used for atomic writes next to `file_path`.
fn tmp_path_for(file_path: &Path) -> PathBuf {
    let dir = file_path.parent().unwrap_or(Path::new("."));
    let file_name = file_path
        .file_name()
        .unwrap_or(std::ffi::OsStr::new("card"))
        .to_string_lossy();
    dir.join(format!(".{file_name}.hashcards-edit.tmp"))
}

/// Write `content` to `file_path` atomically (tmp file + rename).
pub(crate) fn write_atomic(file_path: &Path, content: &str) -> Fallible<()> {
    let tmp = tmp_path_for(file_path);
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, file_path)?;
    Ok(())
}

/// Restore a file to its pre-edit content after a rejected edit.
///
/// A failure here means the rejected edit is still on disk, so the error
/// says so explicitly instead of being swallowed.
pub(crate) fn revert_file(file_path: &Path, original: &str) -> Fallible<()> {
    write_atomic(file_path, original).map_err(|e| {
        ErrorReport::new(format!(
            "Failed to revert {} after a rejected edit: {e}. The file may be inconsistent — check it by hand before editing again.",
            file_path.display()
        ))
    })
}

// ── Hash migration ────────────────────────────────────────────────────────────

/// The result of matching a block's pre-edit cards against its re-parsed cards.
pub struct MigrationPlan {
    /// `(old, new)` hash pairs that unambiguously refer to the same card.
    pub renames: Vec<(CardHash, CardHash)>,
    /// New hashes with no unambiguous old counterpart; inserted fresh.
    pub fresh: Vec<CardHash>,
    /// How many fresh cards may have lost history (old history existed but
    /// could not be matched unambiguously). Reported to the user via flash.
    pub skipped: usize,
}

/// Join a `MigrationPlan` with the re-parsed cards, in the terms a live
/// session needs.
///
/// `plan.renames` already pairs old and new *hashes*; this looks each new
/// hash up among the re-parsed cards so the session gets the whole card,
/// and works out which old cards the edit removed: the ones that were
/// neither renamed nor still present under their own hash.
pub(crate) fn build_card_migration(
    plan: &MigrationPlan,
    old_cards: &[&Card],
    new_cards: &[Card],
) -> CardMigration {
    let by_hash: HashMap<CardHash, &Card> = new_cards.iter().map(|c| (c.hash(), c)).collect();
    let renamed: Vec<(CardHash, Card)> = plan
        .renames
        .iter()
        .filter_map(|(old, new)| by_hash.get(new).map(|card| (*old, (*card).clone())))
        .collect();
    let renamed_from: HashSet<CardHash> = plan.renames.iter().map(|(old, _)| *old).collect();
    let removed: Vec<CardHash> = old_cards
        .iter()
        .map(|c| c.hash())
        .filter(|h| !by_hash.contains_key(h) && !renamed_from.contains(h))
        .collect();
    CardMigration { renamed, removed }
}

/// The matching key for a card: the cloze deletion's text for cloze cards
/// (byte positions, sliced as bytes — never chars), `None` for basic cards.
fn migration_key(card: &Card) -> Option<String> {
    match card.content() {
        CardContent::Cloze { text, start, end } => {
            // start/end are byte positions; end is inclusive.
            text.get(*start..*end + 1).map(str::to_string)
        }
        CardContent::Basic { .. } => None,
    }
}

/// Match old cards against new cards by content, not document order.
///
/// A pair is unambiguous when exactly one changed old card and exactly one
/// changed new card share the same key. Everything else is inserted fresh;
/// if old history went unmatched in the process, the fresh cards are counted
/// as skipped so the user can be told.
pub(crate) fn plan_hash_migration(old_cards: &[&Card], new_cards: &[&Card]) -> MigrationPlan {
    // Hashes present on both sides are unchanged cards: no work needed.
    let old_set: BTreeSet<CardHash> = old_cards.iter().map(|c| c.hash()).collect();
    let new_set: BTreeSet<CardHash> = new_cards.iter().map(|c| c.hash()).collect();
    let changed_old: Vec<&Card> = old_cards
        .iter()
        .copied()
        .filter(|c| !new_set.contains(&c.hash()))
        .collect();
    let changed_new: Vec<&Card> = new_cards
        .iter()
        .copied()
        .filter(|c| !old_set.contains(&c.hash()))
        .collect();

    let mut old_by_key: BTreeMap<Option<String>, Vec<CardHash>> = BTreeMap::new();
    for c in &changed_old {
        old_by_key
            .entry(migration_key(c))
            .or_default()
            .push(c.hash());
    }
    let mut new_by_key: BTreeMap<Option<String>, Vec<CardHash>> = BTreeMap::new();
    for c in &changed_new {
        new_by_key
            .entry(migration_key(c))
            .or_default()
            .push(c.hash());
    }

    let mut renames: Vec<(CardHash, CardHash)> = Vec::new();
    let mut fresh: Vec<CardHash> = Vec::new();
    for (key, new_hashes) in &new_by_key {
        match old_by_key.get(key) {
            Some(old_hashes) if old_hashes.len() == 1 && new_hashes.len() == 1 => {
                renames.push((old_hashes[0], new_hashes[0]));
            }
            _ => fresh.extend(new_hashes.iter().copied()),
        }
    }

    // If any changed old card found no rename partner, the fresh inserts may
    // represent lost history; report them.
    let skipped = if changed_old.len() > renames.len() {
        fresh.len()
    } else {
        0
    };

    MigrationPlan {
        renames,
        fresh,
        skipped,
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn find_card_by_hash(cards: &[Card], hash: CardHash) -> Fallible<&Card> {
    cards.iter().find(|c| c.hash() == hash).ok_or_else(|| {
        ErrorReport::new("Card not found in collection (may have been edited or deleted)")
    })
}

pub(crate) fn file_mtime_ms(path: &Path) -> Fallible<u64> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta.modified()?;
    let ms = mtime
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ErrorReport::new("file mtime predates UNIX epoch"))?
        .as_millis() as u64;
    Ok(ms)
}

fn error_page(slug: &str, hash_hex: &str, msg: &str) -> (StatusCode, Html<String>) {
    let html = page_template(html! {
        div.error {
            h1 { "Error" }
            p { (msg) }
            div.edit-controls {
                a.btn.btn-secondary
                    href=(format!("/collection/{slug}/edit/{hash_hex}"))
                { "\u{2190} Back" }
                " "
                a.btn.btn-secondary
                    href=(format!("/collection/{slug}/bookmarks"))
                { "Bookmarks" }
            }
        }
    })
    .into_string();
    (StatusCode::INTERNAL_SERVER_ERROR, Html(html))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::cmd::serve::handlers::find_collection;
    use crate::types::card::CardContent;
    use crate::types::timestamp::Timestamp;

    /// A closed set, not a caller-supplied path: a redirect target taken
    /// from a form field is an open redirect for no benefit.
    #[test]
    fn return_to_is_a_closed_set() {
        assert_eq!(
            ReturnTo::parse(Some("collection")).target("Spanish"),
            "/collection/Spanish"
        );
        assert_eq!(
            ReturnTo::parse(Some("bookmarks")).target("Spanish"),
            "/collection/Spanish/bookmarks"
        );
        // Anything else is the bookmarks page, not a 400 and not the value.
        for raw in [
            None,
            Some(""),
            Some("https://evil.example.com"),
            Some("//x"),
        ] {
            assert_eq!(
                ReturnTo::parse(raw).target("Spanish"),
                "/collection/Spanish/bookmarks",
                "for {raw:?}"
            );
        }
    }

    /// A state whose card tree holds one collection named `Deck`, with
    /// `files` written into it and a stable id stamped so the read paths can
    /// find it. Returns the state, the collection, and its folder.
    fn fixture(
        data_dir: &Path,
        files: &[(&str, &str)],
    ) -> Fallible<(AppState, ResolvedCollection, PathBuf)> {
        use crate::cmd::serve::cards::CardRoot;
        use crate::cmd::serve::cards::collection_id;
        use crate::cmd::serve::state::test_support::state_with_data_dir;

        let root = CardRoot::for_user(data_dir, None)?;
        let folder = root.path().join("Deck");
        std::fs::create_dir_all(&folder)?;
        for (rel, content) in files {
            std::fs::write(folder.join(rel), content)?;
        }
        std::fs::create_dir_all(data_dir.join("db"))?;
        collection_id(&folder)?;

        let state = state_with_data_dir(data_dir.to_path_buf());
        let rc = find_collection(&state, "Deck", None)
            .ok_or_else(|| ErrorReport::new("the collection was not discovered"))?;
        Ok((state, rc, folder))
    }

    /// The pencil in a running drill links to the slug the *session* is
    /// filed under, and a custom deck's session is filed under the deck's
    /// own slug. Resolving that as a collection found nothing, so every card
    /// edited from a deck drill answered "Unknown collection: deck-..." with
    /// a 500. The card's home is one of the deck's members, so the members
    /// are what get searched.
    #[test]
    fn a_card_is_editable_through_the_deck_it_was_drilled_under() -> Fallible<()> {
        use crate::cmd::serve::config::DeckMember;
        use crate::cmd::serve::decks::ResolvedCustomDeck;
        use crate::cmd::serve::decks::slug_for_deck;

        let dir = tempfile::tempdir()?;
        let data_dir = dir.path().canonicalize()?;
        let (state, _rc, coll_dir) = fixture(&data_dir, &[("Cards.md", "Q: One\nA: 1\n")])?;
        let cards = parse_deck(&coll_dir)?.cards;
        let hash_hex = cards[0].hash().to_hex();

        let deck = ResolvedCustomDeck {
            name: "Mixed".to_string(),
            slug: slug_for_deck("Mixed", None),
            owner: None,
            members: vec![
                DeckMember::parse("Deck/Cards")
                    .ok_or_else(|| ErrorReport::new("the deck member did not parse"))?,
            ],
        };
        state.custom_decks.lock().push(deck.clone());

        let html = edit_get_inner(&state, &deck.slug, &hash_hex, None, ReturnTo::Collection)?;
        assert!(
            html.contains("Q: One"),
            "the editor must open on the card, got: {html}"
        );
        // Back to the drill it was opened from, not to the home collection:
        // the session is filed under the deck's slug.
        assert!(
            html.contains(&format!(r#"href="/collection/{}""#, deck.slug)),
            "the back link must return to the deck, got: {html}"
        );

        // Saving reaches the file in the card's home collection.
        let file = coll_dir.join("Cards.md");
        let mtime = file_mtime_ms(&file)?;
        edit_post_inner(
            &state,
            &deck.slug,
            &hash_hex,
            EditForm {
                new_text: "Q: One\nA: uno".to_string(),
                mtime_ms: mtime.to_string(),
                return_to: Some("collection".to_string()),
            },
            None,
        )?;
        assert!(std::fs::read_to_string(&file)?.contains("A: uno"));
        Ok(())
    }

    /// Regression: an edit that makes a card identical to one that already
    /// has history must not leave the file saved, the database half-migrated
    /// and the user looking at a 500. `rename_card_hash` errors when the
    /// target hash exists, and the migration used to run unguarded, one
    /// statement at a time, after the file had already been written.
    #[test]
    fn test_edit_colliding_with_existing_card_does_not_corrupt() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let data_dir = dir.path().canonicalize()?;
        // Two distinct cards, both with history.
        let (state, rc, coll_dir) = fixture(
            &data_dir,
            &[("Deck.md", "Q: One\nA: 1\n\n---\n\nQ: Two\nA: 2\n")],
        )?;
        let file = coll_dir.join("Deck.md");
        let slug = rc.slug.clone();

        let old_cards = parse_deck(&coll_dir)?.cards;
        assert_eq!(old_cards.len(), 2);
        {
            let db = open_collection_db(&state, &rc)?;
            let now = Timestamp::now();
            for c in &old_cards {
                db.insert_card_if_new(c.hash(), now)?;
            }
        }

        // Edit the first card into a byte-identical copy of the second, whose
        // hash already exists in the database.
        let mtime = file_mtime_ms(&file)?;
        let outcome = edit_post_inner(
            &state,
            &slug,
            &old_cards[0].hash().to_hex(),
            EditForm {
                new_text: "Q: Two\nA: 2".to_string(),
                mtime_ms: mtime.to_string(),
                return_to: None,
            },
            None,
        )?;

        // The collision is reported, not raised as a 500.
        assert_eq!(outcome.migrated, 0);
        assert_eq!(outcome.skipped, 1);

        // The second card keeps its history, and nothing was left dangling.
        let db = open_collection_db(&state, &rc)?;
        assert!(db.card_exists(old_cards[1].hash())?);
        Ok(())
    }

    /// Regression: adding a line above the card's own `Q:` shifts the card
    /// down inside the edited block. Matching new cards by exact start line
    /// then found nothing, so no rename was planned, `skipped` stayed 0, and
    /// the user was told "Card saved." while the FSRS history was silently
    /// orphaned to the old hash.
    #[test]
    fn test_edit_prepending_a_line_still_migrates_history() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let data_dir = dir.path().canonicalize()?;
        let (state, rc, coll_dir) = fixture(
            &data_dir,
            &[("Deck.md", "Q: Capital of France?\nA: Paris\n")],
        )?;
        let file = coll_dir.join("Deck.md");
        let slug = rc.slug.clone();

        let old_cards = parse_deck(&coll_dir)?.cards;
        assert_eq!(old_cards.len(), 1);
        let old_hash = old_cards[0].hash();
        let hash_hex = old_hash.to_hex();

        {
            let db = open_collection_db(&state, &rc)?;
            db.insert_card_if_new(old_hash, Timestamp::now())?;
        }

        // A prose line above the Q: pushes the card one line down.
        let mtime = file_mtime_ms(&file)?;
        let outcome = edit_post_inner(
            &state,
            &slug,
            &hash_hex,
            EditForm {
                new_text: "## Geography\nQ: Capital of France?\nA: Paris, France".to_string(),
                mtime_ms: mtime.to_string(),
                return_to: None,
            },
            None,
        )?;
        assert_eq!(
            outcome.migrated, 1,
            "the shifted card must still be matched to its history"
        );
        assert_eq!(outcome.skipped, 0);

        let new_cards = parse_deck(&coll_dir)?.cards;
        assert_eq!(new_cards.len(), 1);
        let db = open_collection_db(&state, &rc)?;
        assert!(
            db.card_exists(new_cards[0].hash())?,
            "history must have followed the card to its new hash"
        );
        assert!(!db.card_exists(old_hash)?, "the old row must not linger");
        Ok(())
    }

    /// Regression test for BUG-35, end to end: reordering the deletions of a
    /// cloze card migrates both histories instead of orphaning them.
    #[test]
    fn test_edit_reorder_migrates_history_by_content() -> Fallible<()> {
        let dir = tempfile::tempdir()?;
        let data_dir = dir.path().canonicalize()?;
        let (state, rc, coll_dir) = fixture(&data_dir, &[("Deck.md", "C: A [x] B [y]\n")])?;
        let file = coll_dir.join("Deck.md");
        let slug = rc.slug.clone();

        let old_cards = parse_deck(&coll_dir)?.cards;
        assert_eq!(old_cards.len(), 2);
        let hash_hex = old_cards[0].hash().to_hex();

        // Seed the DB with both pre-edit cards.
        {
            let db = open_collection_db(&state, &rc)?;
            let now = Timestamp::now();
            for c in &old_cards {
                db.insert_card_if_new(c.hash(), now)?;
            }
        }

        let mtime = file_mtime_ms(&file)?;
        let outcome = edit_post_inner(
            &state,
            &slug,
            &hash_hex,
            EditForm {
                new_text: "C: A [y] B [x]".to_string(),
                mtime_ms: mtime.to_string(),
                return_to: None,
            },
            None,
        )?;
        assert_eq!(outcome.migrated, 2);
        assert_eq!(outcome.skipped, 0);

        // Every post-edit hash is in the DB; no pre-edit hash remains.
        let db = open_collection_db(&state, &rc)?;
        let new_cards = parse_deck(&coll_dir)?.cards;
        assert_eq!(new_cards.len(), 2);
        for c in &new_cards {
            assert!(db.card_exists(c.hash())?, "missing new card {}", c.hash());
        }
        for c in &old_cards {
            assert!(!db.card_exists(c.hash())?, "stale old card {}", c.hash());
        }
        Ok(())
    }

    fn cloze(text: &str, start: usize, end: usize) -> Card {
        Card::new(
            "Deck".to_string(),
            PathBuf::from("/tmp/deck.md"),
            (0, 0),
            CardContent::new_cloze(text, start, end),
        )
    }

    /// Regression test for BUG-35: reordering deletions must pair history by
    /// deletion content, not document order.
    #[test]
    fn test_migration_matches_by_deletion_content_not_order() {
        // "A x B y": deletion "x" at bytes 2..=2, "y" at 6..=6.
        let old_x = cloze("A x B y", 2, 2);
        let old_y = cloze("A x B y", 6, 6);
        // Edited to "A y B x": deletion order swapped.
        let new_y = cloze("A y B x", 2, 2);
        let new_x = cloze("A y B x", 6, 6);

        let plan = plan_hash_migration(&[&old_x, &old_y], &[&new_y, &new_x]);

        assert_eq!(plan.skipped, 0);
        assert!(plan.fresh.is_empty());
        assert_eq!(plan.renames.len(), 2);
        // "x" history follows the "x" deletion, "y" follows "y".
        assert!(plan.renames.contains(&(old_x.hash(), new_x.hash())));
        assert!(plan.renames.contains(&(old_y.hash(), new_y.hash())));
    }

    #[test]
    fn test_migration_skips_ambiguous_duplicate_deletions() {
        // Two deletions with identical text "x" — no unambiguous pairing.
        let old_a = cloze("x and x", 0, 0);
        let old_b = cloze("x and x", 6, 6);
        let new_a = cloze("x plus x", 0, 0);
        let new_b = cloze("x plus x", 7, 7);

        let plan = plan_hash_migration(&[&old_a, &old_b], &[&new_a, &new_b]);

        assert!(plan.renames.is_empty());
        assert_eq!(plan.fresh.len(), 2);
        assert_eq!(plan.skipped, 2);
    }

    #[test]
    fn test_migration_unchanged_cards_are_untouched() {
        let old = cloze("A x B", 2, 2);
        let new = cloze("A x B", 2, 2);
        let plan = plan_hash_migration(&[&old], &[&new]);
        assert!(plan.renames.is_empty());
        assert!(plan.fresh.is_empty());
        assert_eq!(plan.skipped, 0);
    }

    #[test]
    fn test_migration_pairs_single_basic_card() {
        let old = Card::new(
            "Deck".to_string(),
            PathBuf::from("/tmp/deck.md"),
            (0, 1),
            CardContent::new_basic("Q old", "A old"),
        );
        let new = Card::new(
            "Deck".to_string(),
            PathBuf::from("/tmp/deck.md"),
            (0, 1),
            CardContent::new_basic("Q new", "A new"),
        );
        let plan = plan_hash_migration(&[&old], &[&new]);
        assert_eq!(plan.renames, vec![(old.hash(), new.hash())]);
        assert!(plan.fresh.is_empty());
        assert_eq!(plan.skipped, 0);
    }

    #[test]
    fn test_migration_pure_addition_is_not_skipped() {
        // A brand-new deletion with no old history to lose.
        let new = cloze("A x B", 2, 2);
        let plan = plan_hash_migration(&[], &[&new]);
        assert!(plan.renames.is_empty());
        assert_eq!(plan.fresh, vec![new.hash()]);
        assert_eq!(plan.skipped, 0);
    }

    #[test]
    fn test_block_end_terminator() {
        let lines = vec!["Q: foo", "A: bar", "---", "Q: baz"];
        // Card at (0, 2): range.1 = 2, lines[2] = "---" → terminator
        assert_eq!(block_end(&lines, (0, 2)), 2);
    }

    #[test]
    fn test_block_end_eof() {
        let lines = vec!["Q: foo", "A: bar"];
        // Card at (0, 1): range.1 = 1, lines[1] = "A: bar" → not a terminator
        assert_eq!(block_end(&lines, (0, 1)), 2);
    }

    /// A one-line card (e.g. a single-line `C:` cloze at EOF) has
    /// `range.0 == range.1`; its own first line must not be mistaken for the
    /// terminator of the block, or the splice inserts instead of replacing.
    #[test]
    fn test_block_end_single_line_card_is_not_its_own_terminator() {
        let lines = vec!["C: A [x] B [y]"];
        assert_eq!(block_end(&lines, (0, 0)), 1);
    }

    #[test]
    fn test_block_end_next_card() {
        let lines = vec!["Q: foo", "A: bar", "Q: baz", "A: quux"];
        // First card at (0, 2): range.1 = 2, lines[2] = "Q: baz" → terminator
        assert_eq!(block_end(&lines, (0, 2)), 2);
    }

    #[test]
    fn test_extract_card_block_no_fm() {
        let content = "Q: foo\nA: bar\n---\nQ: baz\nA: quux";
        let block = extract_card_block(content, (0, 2)).unwrap();
        assert_eq!(block, "Q: foo\nA: bar");
    }

    #[test]
    fn test_extract_card_block_eof() {
        let content = "Q: foo\nA: bar";
        let block = extract_card_block(content, (0, 1)).unwrap();
        assert_eq!(block, "Q: foo\nA: bar");
    }

    #[test]
    fn test_splice_basic() {
        let dir = std::env::temp_dir().join("hashcards_edit_test_splice");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.md");
        let original = "Q: foo\nA: bar\n---\nQ: baz\nA: quux\n";
        std::fs::write(&path, original).unwrap();

        let mtime = file_mtime_ms(&path).unwrap();
        splice_card_block(
            &path,
            original,
            (0, 2),
            "Q: foo edited\nA: bar edited",
            mtime,
        )
        .unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            result,
            "Q: foo edited\nA: bar edited\n---\nQ: baz\nA: quux\n"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_splice_eof() {
        let dir = std::env::temp_dir().join("hashcards_edit_test_eof");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.md");
        let original = "Q: foo\nA: bar\n";
        std::fs::write(&path, original).unwrap();

        let mtime = file_mtime_ms(&path).unwrap();
        splice_card_block(
            &path,
            original,
            (0, 1),
            "Q: foo edited\nA: bar edited",
            mtime,
        )
        .unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        assert_eq!(result, "Q: foo edited\nA: bar edited\n");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_revert_file_restores_content() {
        let dir = std::env::temp_dir().join("hashcards_edit_test_revert_ok");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.md");
        std::fs::write(&path, "garbled").unwrap();

        revert_file(&path, "Q: foo\nA: bar\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Q: foo\nA: bar\n");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn test_revert_failure_reports_inconsistent_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join("hashcards_edit_test_revert_fail");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.md");
        std::fs::write(&path, "Q: foo\nA: bar\n").unwrap();
        // Read-only directory: the atomic tmp-file write must fail.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = revert_file(&path, "Q: foo\nA: bar\n");

        // Restore permissions before asserting so cleanup always works.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("may be inconsistent"),
            "unexpected message: {msg}"
        );
    }

    #[test]
    fn test_splice_rejects_stale_mtime() {
        let dir = std::env::temp_dir().join("hashcards_edit_test_stale_mtime");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.md");
        let original = "Q: foo\nA: bar\n";
        std::fs::write(&path, original).unwrap();
        let mtime = file_mtime_ms(&path).unwrap();

        // Pretend the form was opened against a different (older) mtime.
        let result = splice_card_block(&path, original, (0, 1), "Q: x\nA: y", mtime + 1);

        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("changed on disk"), "unexpected message: {msg}");
        // The file must be untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_splice_with_frontmatter_absolute_range() {
        let dir = std::env::temp_dir().join("hashcards_edit_test_fm");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.md");
        // Q: is on absolute 0-based line 3, A: on line 4.
        let original = "---\nname = \"X\"\n---\nQ: foo\nA: bar\n";
        std::fs::write(&path, original).unwrap();

        let mtime = file_mtime_ms(&path).unwrap();
        splice_card_block(
            &path,
            original,
            (3, 4),
            "Q: foo edited\nA: bar edited",
            mtime,
        )
        .unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            result,
            "---\nname = \"X\"\n---\nQ: foo edited\nA: bar edited\n"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
